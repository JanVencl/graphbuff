//! `get_schema`: what a model needs to write Cypher for a graph, in one call.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use graphbuff::{GraphSchema, PropertySchema, QueryOptions};

use crate::json::{self, Json};
use crate::users::Access;

use super::render;
use super::rpc::string;
use super::tools::{Input, Output};

/// How long a database's schema is kept.
const TTL: Duration = Duration::from_secs(60);
/// Nodes per label and relationships per type the properties are learnt from.
const SAMPLE: usize = 1000;

/// What a database's schema was learnt from, and when.
struct Entry {
    at: Instant,
    /// Node and relationship counts then: a change in them makes the entry stale.
    counts: (usize, usize),
    schema: Arc<GraphSchema>,
    indexes: Json,
    constraints: Json,
}

/// The schemas of the databases, kept for [`TTL`].
#[derive(Default)]
pub(crate) struct Cache {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
}

fn arg_labels(i: &Input) -> Result<Option<Vec<String>>, String> {
    if let Some(k) = i.args.keys().find(|k| !["database", "labels"].contains(&k.as_str())) {
        return Err(format!("unknown argument `{k}`; the arguments are database, labels"));
    }
    match i.args.get("labels") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|l| match l {
                Json::String(s) => Ok(s.clone()),
                _ => Err("`labels` must be a list of label names".to_string()),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err("`labels` must be a non-empty list of label names".into()),
    }
}

/// The rows of `query` (a procedure call) as objects.
fn objects(db: &graphbuff::Database<graphbuff::BoxedStore>, query: &str, options: &QueryOptions) -> Result<Json, String> {
    let r = db.execute_with_options(query, &BTreeMap::new(), options).map_err(|e| render::error_text(&e))?;
    let rows = r
        .rows
        .iter()
        .map(|row| {
            let mut text = String::new();
            json::write_row(&mut text, row);
            let values = match json::parse(&text) {
                Ok(Json::Array(v)) => v,
                _ => Vec::new(),
            };
            Json::Object(
                r.columns.iter().cloned().zip(values).filter(|(_, v)| !matches!(v, Json::Null)).collect(),
            )
        })
        .collect();
    Ok(Json::Array(rows))
}

/// `{"name": "String", "born": "Long?"}`: types joined by `|`, `?` when not every
/// sampled entity has the property.
fn properties(props: &[PropertySchema]) -> Json {
    Json::Object(
        props
            .iter()
            .map(|p| {
                let mut t = p.types.join("|");
                if !p.mandatory {
                    t.push('?');
                }
                (p.name.clone(), Json::String(t))
            })
            .collect(),
    )
}

fn quote(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit()) {
        name.to_string()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

/// The text for the model. `with_properties: false` leaves the properties out (for a
/// schema too big to show whole).
fn render(db: &str, e: &Entry, labels: Option<&[String]>, with_properties: bool) -> String {
    let s = &e.schema;
    let wanted = |l: &str| labels.is_none_or(|ls| ls.iter().any(|x| x == l));
    let patterns: Vec<_> = s.patterns.iter().filter(|p| wanted(&p.start) || wanted(&p.end)).collect();
    let types: Vec<&str> = patterns.iter().map(|p| p.rel_type.as_str()).collect();
    let entity = |name: &str, key: &str, count: usize, props: &[PropertySchema]| {
        let mut o = BTreeMap::from([(key.to_string(), string(name)), ("count".to_string(), Json::Int(count as i64))]);
        if with_properties {
            o.insert("properties".into(), properties(props));
        }
        Json::Object(o)
    };
    let mut out = vec![
        ("database", string(db)),
        (
            "labels",
            Json::Array(s.labels.iter().filter(|l| wanted(&l.name)).map(|l| entity(&l.name, "label", l.count, &l.properties)).collect()),
        ),
        (
            "relationshipTypes",
            Json::Array(
                s.relationship_types
                    .iter()
                    .filter(|t| labels.is_none() || types.contains(&t.name.as_str()))
                    .map(|t| entity(&t.name, "type", t.count, &t.properties))
                    .collect(),
            ),
        ),
        (
            "patterns",
            Json::Array(
                patterns
                    .iter()
                    .map(|p| string(format!("(:{})-[:{}]->(:{})", quote(&p.start), quote(&p.rel_type), quote(&p.end))))
                    .collect(),
            ),
        ),
    ];
    let about = |o: &Json, key: &str| match o {
        Json::Object(m) => match m.get(key) {
            Some(Json::String(l)) => wanted(l) || labels.is_none(),
            _ => labels.is_none(),
        },
        _ => true,
    };
    let keep = |list: &Json, key: &str| match list {
        Json::Array(items) => Json::Array(items.iter().filter(|o| about(o, key)).cloned().collect()),
        other => other.clone(),
    };
    out.push(("indexes", keep(&e.indexes, "labelOrType")));
    out.push(("constraints", keep(&e.constraints, "label")));
    let partial = s.labels.iter().any(|l| l.sampled < l.count) || s.relationship_types.iter().any(|t| t.sampled < t.count);
    let mut notes = Vec::new();
    if with_properties {
        notes.push(String::from(
            "property types: `?` means not every node or relationship has it; `|` lists several types",
        ));
        if partial {
            notes.push(format!("properties and patterns come from up to {SAMPLE} nodes per label and relationships per type; counts are exact"));
        }
    } else {
        notes.push("the schema is too big to show whole: properties are left out; pass `labels` to see theirs".into());
    }
    out.push(("notes", Json::Array(notes.into_iter().map(Json::String).collect())));
    // Keys in this order, not sorted.
    let mut text = String::from("{");
    for (i, (k, v)) in out.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        json::write_str(&mut text, k);
        text.push(':');
        super::rpc::write(&mut text, v);
    }
    text.push('}');
    text
}

pub(crate) fn get_schema(i: &Input) -> Output {
    let ctx = i.ctx;
    let labels = match arg_labels(i) {
        Ok(l) => l,
        Err(e) => return Output::error(e),
    };
    let name = match i.args.get("database") {
        None | Some(Json::Null) => ctx.mgr.default_database(),
        Some(Json::String(d)) => match ctx.mgr.database(d) {
            Ok(_) => ctx.mgr.resolve_name(&d.to_ascii_lowercase()),
            Err(_) => return Output::error(format!("database `{d}` does not exist; list_databases gives those you may use")),
        },
        Some(_) => return Output::error("`database` must be a string"),
    };
    if super::cap(ctx.access_on(i.who, &name)) < Access::Read {
        return Output::error(format!("you may not read database `{name}`; list_databases gives those you may use"));
    }
    let db = match ctx.mgr.database(&name) {
        Ok(d) => d,
        Err(e) => return Output::error(render::error_text(&e)),
    };
    let counts = (db.node_count(), db.relationship_count());
    let cached = i.schemas.entries.lock().unwrap_or_else(|e| e.into_inner()).get(&name).cloned();
    let entry = match cached {
        Some(e) if e.at.elapsed() < TTL && e.counts == counts => e,
        _ => {
            let mut options = ctx.query_options(Some(i.limits.query_timeout));
            options.cancel = Some(i.cancel.clone());
            let learnt = db.schema(SAMPLE, &options).map_err(|e| render::error_text(&e)).and_then(|schema| {
                Ok(Entry {
                    at: Instant::now(),
                    counts,
                    schema: Arc::new(schema),
                    indexes: objects(&db, "CALL db.indexes()", &options)?,
                    constraints: objects(&db, "CALL db.constraints()", &options)?,
                })
            });
            match learnt {
                Ok(e) => {
                    let e = Arc::new(e);
                    i.schemas.entries.lock().unwrap_or_else(|e| e.into_inner()).insert(name.clone(), Arc::clone(&e));
                    e
                }
                Err(e) => return Output::error(e),
            }
        }
    };
    if let Some(ls) = &labels {
        let unknown: Vec<&str> =
            ls.iter().filter(|l| !entry.schema.labels.iter().any(|x| &x.name == *l)).map(String::as_str).collect();
        if !unknown.is_empty() {
            let known: Vec<&str> = entry.schema.labels.iter().map(|l| l.name.as_str()).collect();
            return Output::error(format!(
                "no nodes have the label(s) {}; the labels are: {}",
                unknown.join(", "),
                if known.is_empty() { "none (the database is empty)".to_string() } else { known.join(", ") }
            ));
        }
    }
    let text = render(&name, &entry, labels.as_deref(), true);
    if text.len() <= i.limits.max_bytes || labels.is_some() {
        return Output::ok(text);
    }
    Output::ok(render(&name, &entry, None, false))
}

#[cfg(test)]
mod tests {
    #[test]
    fn quoting() {
        assert_eq!(super::quote("Person"), "Person");
        assert_eq!(super::quote("ACTED_IN"), "ACTED_IN");
        assert_eq!(super::quote("my label"), "`my label`");
        assert_eq!(super::quote("1x"), "`1x`");
    }
}
