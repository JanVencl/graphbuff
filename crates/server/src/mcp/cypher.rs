//! `read_cypher` and `write_cypher`, and what MCP lets through.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use graphbuff::cypher::ast::{Query, Statement};
use graphbuff::{Error, QueryOptions, Value};

use crate::api;
use crate::json::{Json, to_value};
use crate::users::{self, Access};

use super::render::{self, Capped};
use super::tools::{Input, Output};

/// The longest a dry run may hold its database's write lock.
const DRY_RUN_TIMEOUT: Duration = Duration::from_secs(10);

/// The arguments of the Cypher tools.
struct Args {
    query: String,
    params: BTreeMap<String, Value>,
    database: Option<String>,
    max_rows: usize,
    dry_run: bool,
}

fn args(i: &Input, write: bool) -> Result<Args, String> {
    let allowed: &[&str] = if write {
        &["query", "params", "database", "max_rows", "dry_run"]
    } else {
        &["query", "params", "database", "max_rows"]
    };
    if let Some(k) = i.args.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("unknown argument `{k}`; the arguments are {}", allowed.join(", ")));
    }
    let query = match i.args.get("query") {
        Some(Json::String(q)) if !q.trim().is_empty() => q.clone(),
        _ => return Err("`query` (a Cypher statement) is required".into()),
    };
    let params = match i.args.get("params") {
        None | Some(Json::Null) => BTreeMap::new(),
        Some(Json::Object(m)) => m.iter().map(|(k, v)| (k.clone(), to_value(v.clone()))).collect(),
        Some(_) => return Err("`params` must be an object of parameter names and values".into()),
    };
    let database = match i.args.get("database") {
        None | Some(Json::Null) => None,
        Some(Json::String(d)) => Some(d.clone()),
        Some(_) => return Err("`database` must be a string".into()),
    };
    let limit = i.limits.max_rows_limit;
    let max_rows = match i.args.get("max_rows") {
        None | Some(Json::Null) => i.limits.max_rows,
        Some(Json::Int(n)) if *n >= 1 && (*n as u64) <= limit as u64 => *n as usize,
        Some(_) => return Err(format!("`max_rows` must be a whole number from 1 to {limit}")),
    };
    let dry_run = match i.args.get("dry_run") {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(_) => return Err("`dry_run` must be true or false".into()),
    };
    Ok(Args { query, params, database, max_rows, dry_run })
}

/// Whether `query` reads CSV anywhere, also in subqueries. Over MCP it is refused: a
/// prompt injected into the data could make it send data out in a URL.
pub(crate) fn uses_load_csv(query: &Query) -> bool {
    // The query's debug form names every clause, at any depth. Text inside quotes (string
    // literals, names) is skipped, so a literal cannot hide or fake one.
    let text = format!("{query:?}");
    let mut in_string = false;
    let mut escaped = false;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
        } else if b == b'"' {
            in_string = true;
        } else if text[i..].starts_with("LoadCsv {") && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
            return true;
        }
    }
    false
}

/// Checks that `stmt` may run over MCP, by `write_cypher` (`write`) or `read_cypher`, and
/// returns the database it runs on and the query.
fn allowed(i: &Input, stmt: Statement, current: &str, params: &BTreeMap<String, Value>, write: bool) -> Result<(String, Query), String> {
    let ctx = i.ctx;
    let (use_db, query) = match stmt {
        Statement::Query { use_db, query } => (use_db, query),
        Statement::Use(_) => {
            return Err("USE on its own does nothing here: pass `database`, or start the query with USE db".into());
        }
        Statement::Database(cmd) if cmd.is_show() => {
            return Err("SHOW DATABASES is not available here; call list_databases instead".into());
        }
        Statement::Database(cmd) => return Err(format!("{} is administration, which MCP does not allow", cmd.name())),
        Statement::User(cmd) => return Err(format!("{} is administration, which MCP does not allow", cmd.name())),
        Statement::Transaction(cmd) => return Err(format!("{} is not available over MCP", cmd.name())),
        Statement::Settings(_) => return Err("SHOW SETTINGS is not available over MCP".into()),
    };
    if uses_load_csv(&query) {
        return Err("LOAD CSV is not allowed over MCP".into());
    }
    let db = match &use_db {
        Some(n) => users::resolve(n, params, "database name").map_err(|e| render::error_text(&e))?,
        None => current.to_string(),
    };
    if ctx.mgr.database(&db).is_err() {
        return Err(format!("database `{db}` does not exist; list_databases gives those you may use"));
    }
    let (needed, _) = api::authorize_query(&ctx.mgr, &db, &query);
    if needed == Access::Write && !write {
        return Err("this query writes; read_cypher only reads. Use write_cypher (with dry_run to try it first)".into());
    }
    let stmt = Statement::Query { use_db, query };
    ctx.authorize(i.who, &stmt, current, params).map_err(|e| render::error_text(&e))?;
    let Statement::Query { query, .. } = stmt else { unreachable!() };
    Ok((db, query))
}

pub(crate) fn read_cypher(i: &Input) -> Output {
    run(i, false)
}

pub(crate) fn write_cypher(i: &Input) -> Output {
    run(i, true)
}

fn run(i: &Input, write: bool) -> Output {
    let ctx = i.ctx;
    let a = match args(i, write) {
        Ok(a) => a,
        Err(e) => return Output::error(e),
    };
    let current = match &a.database {
        Some(d) => match ctx.mgr.database(d) {
            Ok(_) => ctx.mgr.resolve_name(&d.to_ascii_lowercase()),
            Err(_) => return Output::error(format!("database `{d}` does not exist; list_databases gives those you may use")),
        },
        None => ctx.mgr.default_database(),
    };
    let stmt = match graphbuff::cypher::parse_statement(&a.query) {
        Ok(s) => s,
        Err(e) => {
            ctx.metrics.query("mcp", Duration::ZERO, Some(&e));
            return Output::error(render::error_text(&e));
        }
    };
    let (db_name, query) = match allowed(i, stmt, &current, &a.params, write) {
        Ok(v) => v,
        Err(e) => return Output::error(e),
    };
    if a.dry_run && query.calls_in_transactions() {
        return Output::error("CALL { … } IN TRANSACTIONS commits as it goes, so it cannot be a dry run");
    }
    let db = match ctx.mgr.database(&db_name) {
        Ok(d) => d,
        Err(e) => return Output::error(render::error_text(&e)),
    };
    let mut options: QueryOptions = ctx.query_options(Some(i.limits.query_timeout));
    options.cancel = Some(i.cancel.clone());
    if a.dry_run {
        options.timeout = Some(options.timeout.map_or(DRY_RUN_TIMEOUT, |t| t.min(DRY_RUN_TIMEOUT)));
    }
    let info = crate::running::Info {
        database: db_name.clone(),
        who: i.who.clone(),
        protocol: "mcp",
        connection: "mcp".into(),
        client: i.peer.to_string(),
    };
    let listed = ctx.running.begin(info, crate::running::Stop::Cancel(i.cancel.clone()), Some(&a.query));
    let started = Instant::now();
    // Reads stop at the cap; a write runs to the end (stopping it would roll it back).
    let mut rows = Capped::new(a.max_rows, i.limits.max_bytes, !write);
    let result = if a.dry_run {
        db.begin_with_options(&options).and_then(|mut tx| {
            let r = tx.execute_query_with_options(&query, &a.params, &options);
            tx.rollback();
            r
        })
    } else {
        db.execute_query_streaming(&query, &a.params, &options, &mut rows)
    };
    drop(listed);
    let failure = match &result {
        Err(_) if rows.stopped => None,
        Err(e) => Some(e),
        Ok(_) => None,
    };
    ctx.finished("mcp", &db_name, i.who, &a.query, started.elapsed(), failure);
    match result {
        Ok(r) => {
            let mut extra = vec![("database", Json::String(db_name))];
            if a.dry_run {
                extra.push(("dryRun", Json::Bool(true)));
                extra.push(("rolledBack", Json::Bool(true)));
                rows.feed(&r.columns, r.rows);
            }
            Output::ok(rows.render(Some(&r.stats), r.plan.as_ref(), extra))
        }
        Err(_) if rows.stopped => Output::ok(rows.render(None, None, vec![("database", Json::String(db_name))])),
        Err(Error::Cancelled(_)) if i.cancel.is_cancelled() && !ctx.abort.is_cancelled() => {
            Output::error("the query was cancelled (the client left, or TERMINATE TRANSACTIONS); nothing was changed")
        }
        Err(e) => Output::error(render::error_text(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csv(q: &str) -> bool {
        match graphbuff::cypher::parse_statement(q).unwrap() {
            Statement::Query { query, .. } => uses_load_csv(&query),
            _ => panic!(),
        }
    }

    #[test]
    fn load_csv_anywhere() {
        assert!(csv("LOAD CSV FROM 'https://x' AS r RETURN r"));
        assert!(csv("MATCH (n) CALL { WITH n LOAD CSV FROM 'file:///a' AS r RETURN r } RETURN n"));
        assert!(csv("RETURN COUNT { LOAD CSV FROM 'https://x' AS r RETURN r } AS c"));
        assert!(csv("MATCH (n) RETURN n UNION LOAD CSV FROM 'https://x' AS n RETURN n"));
        assert!(!csv("RETURN 'LoadCsv { }' AS s, \"x\\\"LoadCsv {\" AS t"));
        assert!(!csv("MATCH (n:LoadCsv) RETURN n.LoadCsv"));
    }
}
