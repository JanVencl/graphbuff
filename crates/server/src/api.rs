//! The JSON API: routes and request decoding.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graphbuff::{CancelHandle, DatabaseManager, Error, QueryOptions, QueryResult, Value};

use graphbuff::cypher::ast::{DatabaseCommand, DbName, Query, QueryMode, Statement};

use crate::crypto;
use crate::http::{Request, Response};
use crate::log::Log;
use crate::json::{self, Json, encode_result, to_value, write_str};
use crate::tx::{Transactions, TxError};
use crate::users::{self, Access, Principal, Users};

pub(crate) struct Ctx {
    pub mgr: Arc<DatabaseManager>,
    pub token: Option<String>,
    pub users: Option<Arc<Users>>,
    pub txs: Arc<Transactions>,
    /// The longest a query may run; requests may ask for less.
    pub query_timeout: Option<Duration>,
    /// Where backups go; `None` turns the backup and restore endpoints off.
    pub backup_dir: Option<std::path::PathBuf>,
    pub log: Option<Arc<Log>>,
    /// Cancelled when shutdown stops waiting for queries to finish; every query runs
    /// with it or a child of it.
    pub abort: CancelHandle,
}

impl Ctx {
    /// Logs a finished query: a slow one to the query log, a storage failure as an error.
    pub(crate) fn finished(
        &self,
        protocol: &str,
        database: &str,
        who: &Principal,
        text: &str,
        elapsed: Duration,
        error: Option<&Error>,
    ) {
        let Some(log) = &self.log else { return };
        let user = match who {
            Principal::Open => "-",
            Principal::Token => "token",
            Principal::User(name) => name,
        };
        log.query(elapsed, protocol, database, user, text);
        if let Some(e @ Error::Storage(_)) = error {
            log.error(format_args!("{protocol} query on database {database} failed: {e}"));
        }
    }

    /// Logs a failed login from `peer`.
    pub(crate) fn login_failed(&self, protocol: &str, peer: &str, user: &str) {
        if let Some(log) = &self.log {
            let user = if user.is_empty() { String::new() } else { format!(" as `{user}`") };
            log.warn(format_args!("failed login ({protocol}) from {peer}{user}"));
        }
    }

    /// The limits for a query that asked for `requested` milliseconds (if anything).
    pub(crate) fn query_options(&self, requested: Option<Duration>) -> QueryOptions {
        QueryOptions {
            timeout: match (requested, self.query_timeout) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            cancel: Some(self.abort.clone()),
        }
    }

    /// Whether clients must log in: with a token or with user accounts.
    pub(crate) fn auth_required(&self) -> bool {
        self.token.is_some() || self.users.is_some()
    }

    /// Who `user` with `password` is: a user account whose password it is, or (any
    /// user name) the holder of the server token.
    pub(crate) fn login(&self, user: &str, password: &str) -> Option<Principal> {
        if !self.auth_required() {
            return Some(Principal::Open);
        }
        if let Some(users) = &self.users
            && users.authenticate(user, password)
        {
            return Some(Principal::User(user.to_string()));
        }
        match &self.token {
            Some(t) if same_secret(password, t) => Some(Principal::Token),
            _ => None,
        }
    }

    /// What `who` may do now; `None` if the account no longer exists.
    pub(crate) fn access(&self, who: &Principal) -> Option<Access> {
        match who {
            Principal::Open | Principal::Token => Some(Access::Admin),
            Principal::User(name) => self.users.as_ref()?.access(name),
        }
    }

    /// Runs a user command for `who`.
    pub(crate) fn user_command(
        &self,
        who: &Principal,
        access: Access,
        cmd: &graphbuff::cypher::ast::UserCommand,
        params: &BTreeMap<String, Value>,
    ) -> Result<QueryResult, Error> {
        let Some(users) = &self.users else {
            return Err(Error::Unsupported(format!(
                "{}: user accounts are off; start graphbuff-server with --users FILE",
                cmd.name()
            )));
        };
        users::run(users, who, access, cmd, params)
    }
}

/// Checks that a client that may do `access` may run `stmt` (user commands check for
/// themselves).
pub(crate) fn authorize(access: Access, stmt: &Statement) -> Result<(), Error> {
    let (needed, what) = match stmt {
        Statement::Query { query, .. } => authorize_query(query),
        Statement::Use(_) | Statement::Database(DatabaseCommand::Show(_)) => (Access::Read, "reading"),
        Statement::Database(cmd) => (Access::Admin, cmd.name()),
        Statement::User(_) => return Ok(()),
    };
    check(access, needed, what)
}

/// The access `query` needs, and what it does.
pub(crate) fn authorize_query(query: &Query) -> (Access, &'static str) {
    if query.is_updating() && query.mode != QueryMode::Explain {
        (Access::Write, "writing")
    } else {
        (Access::Read, "reading")
    }
}

pub(crate) fn check(access: Access, needed: Access, what: &str) -> Result<(), Error> {
    if access >= needed {
        return Ok(());
    }
    let role = match needed {
        Access::None | Access::Read => "reader",
        Access::Write => "editor",
        Access::Admin => "admin",
    };
    Err(Error::Forbidden(format!("{what} needs the {role} role (or a stronger one)")))
}

pub(crate) fn handle(ctx: &Ctx, req: &Request, peer: &str) -> Response {
    let segments: Vec<String> = req
        .path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();

    // The health check stays open so that load balancers need no token.
    let who = if segments.as_slice() == ["health"] {
        Principal::Open
    } else {
        match authenticate(ctx, req) {
            Some(who) => who,
            None => {
                ctx.login_failed("http", peer, &basic_user(req).unwrap_or_default());
                let challenge = if ctx.users.is_some() {
                    r#"Basic realm="graphbuff", charset="UTF-8""#
                } else {
                    "Bearer"
                };
                return error(401, "Unauthorized", "missing or wrong credentials")
                    .with_header("WWW-Authenticate", challenge);
            }
        }
    };
    // The account may have been dropped since this connection logged in.
    let Some(access) = ctx.access(&who) else {
        return error(401, "Unauthorized", "the user no longer exists");
    };
    let me = Client { who: &who, access };
    let ctx = &(ctx, me);
    let method = req.method.as_str();
    match segments.as_slice() {
        ["health"] => match method {
            "GET" => Response::json(200, r#"{"status":"ok"}"#.into()),
            _ => not_allowed("GET"),
        },
        ["db"] => match method {
            "GET" => list_databases(ctx),
            _ => not_allowed("GET"),
        },
        ["query"] => match method {
            "POST" => run_query(ctx, None, req),
            _ => not_allowed("POST"),
        },
        ["db", name, "query"] => match method {
            "POST" => run_query(ctx, Some(name), req),
            _ => not_allowed("POST"),
        },
        ["tx"] => match method {
            "POST" => begin(ctx, None, req),
            _ => not_allowed("POST"),
        },
        ["db", name, "tx"] => match method {
            "POST" => begin(ctx, Some(name), req),
            _ => not_allowed("POST"),
        },
        ["tx", id] => in_transaction(ctx, None, id, req),
        ["db", name, "tx", id] => in_transaction(ctx, Some(name), id, req),
        ["tx", id, "commit"] => match method {
            "POST" => commit(ctx, None, id),
            _ => not_allowed("POST"),
        },
        ["db", name, "tx", id, "commit"] => match method {
            "POST" => commit(ctx, Some(name), id),
            _ => not_allowed("POST"),
        },
        ["db", name, "backup"] => match method {
            "POST" => backup(ctx, name, req),
            _ => not_allowed("POST"),
        },
        ["db", name, "restore"] => match method {
            "POST" => restore(ctx, name, req),
            _ => not_allowed("POST"),
        },
        ["db", name, "check"] => match method {
            "POST" => check_database(ctx, name),
            _ => not_allowed("POST"),
        },
        _ => error(404, "NotFound", &format!("no such endpoint: {}", req.path)),
    }
}

/// Who sent `req`: `Authorization: Bearer <token>` or `Basic <base64 user:password>`.
fn authenticate(ctx: &Ctx, req: &Request) -> Option<Principal> {
    if !ctx.auth_required() {
        return Some(Principal::Open);
    }
    let header = req.header("authorization")?.trim();
    let (scheme, value) = header.split_once(' ')?;
    let value = value.trim();
    if scheme.eq_ignore_ascii_case("bearer") {
        return match &ctx.token {
            Some(t) if same_secret(value, t) => Some(Principal::Token),
            _ => None,
        };
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = String::from_utf8(crypto::base64_decode(value)?).ok()?;
        let (user, password) = decoded.split_once(':')?;
        return ctx.login(user, password);
    }
    None
}

/// The user name of `Authorization: Basic …`, for the log.
fn basic_user(req: &Request) -> Option<String> {
    let (scheme, value) = req.header("authorization")?.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(crypto::base64_decode(value.trim())?).ok()?;
    Some(decoded.split_once(':')?.0.to_string())
}

/// The client a request comes from, and what it may do.
#[derive(Clone, Copy)]
struct Client<'a> {
    who: &'a Principal,
    access: Access,
}

/// The server context with the client of the request.
type Req<'a> = (&'a Ctx, Client<'a>);

/// Compares in constant time, so the response time does not reveal a matching prefix.
pub(crate) fn same_secret(given: &str, secret: &str) -> bool {
    let (a, b) = (given.as_bytes(), secret.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- errors -------------------------------------------------------------------------------

pub(crate) fn error(status: u16, code: &str, message: &str) -> Response {
    Response::json(status, json::encode_error_message(code, message))
}

fn not_allowed(allow: &'static str) -> Response {
    error(405, "MethodNotAllowed", &format!("use {allow}")).with_header("Allow", allow)
}

/// Maps an engine error to a status code and an error object.
fn query_error(e: &Error) -> Response {
    let status = match e {
        Error::Syntax { .. }
        | Error::Semantic(_)
        | Error::Type(_)
        | Error::Runtime(_)
        | Error::Unsupported(_) => 400,
        Error::Constraint(_) | Error::Cancelled(_) => 409,
        Error::Storage(_) => 500,
        Error::TransactionTooLarge(_) => 413,
        Error::Timeout(_) => 408,
        Error::Forbidden(_) => 403,
    };
    Response::json(status, json::encode_error(e))
}

// ---- endpoints ----------------------------------------------------------------------------

fn list_databases(&(ctx, me): &Req) -> Response {
    if let Err(e) = check(me.access, Access::Read, "listing databases") {
        return query_error(&e);
    }
    let mut body = String::from(r#"{"databases":["#);
    for (i, db) in ctx.mgr.databases().iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(r#"{"name":"#);
        write_str(&mut body, &db.name);
        body.push_str(r#","storage":"#);
        write_str(&mut body, db.storage.name());
        let _ = write!(
            body,
            r#","default":{},"nodes":{},"relationships":{}}}"#,
            db.default, db.nodes, db.relationships
        );
    }
    body.push_str("]}");
    Response::json(200, body)
}

/// The request body as text, if it is JSON (or has no content type) and UTF-8.
fn json_body(req: &Request) -> Result<&str, Response> {
    if let Some(ct) = req.header("content-type")
        && !ct.to_ascii_lowercase().starts_with("application/json")
    {
        return Err(error(415, "BadRequest", "send the request body as application/json"));
    }
    std::str::from_utf8(&req.body).map_err(|_| error(400, "BadRequest", "the request body is not UTF-8"))
}

fn run_query(&(ctx, me): &Req, db: Option<&str>, req: &Request) -> Response {
    let text = match json_body(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let (query, params, timeout) = match decode_request(text) {
        Ok(v) => v,
        Err(m) => return error(400, "BadRequest", &m),
    };

    let statement = match graphbuff::cypher::parse_statement(&query) {
        Ok(s) => s,
        Err(e) => return query_error(&e),
    };
    if let Statement::User(cmd) = &statement {
        return match ctx.user_command(me.who, me.access, cmd, &params) {
            Ok(r) => Response::json(200, encode_result(&r)),
            Err(e) => query_error(&e),
        };
    }
    if let Err(e) = authorize(me.access, &statement) {
        return query_error(&e);
    }
    let mut session = ctx.mgr.session();
    if let Some(name) = db {
        match session.use_database(name) {
            Ok(()) => {}
            Err(Error::Semantic(m)) => return error(400, "SemanticError", &m),
            Err(_) => {
                return error(404, "DatabaseNotFound", &format!("database `{name}` does not exist"));
            }
        }
    }
    let started = Instant::now();
    let result = session.execute_with_options(&query, &params, &ctx.query_options(timeout));
    ctx.finished("http", session.current_database(), me.who, &query, started.elapsed(), result.as_ref().err());
    match result {
        Ok(r) => Response::json(200, encode_result(&r)),
        Err(e) => query_error(&e),
    }
}

// ---- backup, restore and check --------------------------------------------------------------

/// The fields of a JSON object body: each of `allowed`, if present.
fn body_fields(req: &Request, allowed: &[&str]) -> Result<BTreeMap<String, Json>, Response> {
    let text = json_body(req)?;
    if text.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    match json::parse(text) {
        Ok(Json::Object(m)) => {
            if let Some(k) = m.keys().find(|k| !allowed.contains(&k.as_str())) {
                return Err(error(400, "BadRequest", &format!("unknown field `{k}`")));
            }
            Ok(m)
        }
        Ok(_) => Err(error(400, "BadRequest", "the request body must be a JSON object")),
        Err(e) => Err(error(400, "BadRequest", &e.to_string())),
    }
}

/// The backup directory named by the body's `"backup"` field: one plain name inside
/// the server's backup directory.
fn backup_path(ctx: &Ctx, fields: &BTreeMap<String, Json>) -> Result<std::path::PathBuf, Response> {
    let Some(dir) = &ctx.backup_dir else {
        return Err(error(
            400,
            "Unsupported",
            "backups are off; start graphbuff-server with --backup-dir DIR",
        ));
    };
    let name = match fields.get("backup") {
        Some(Json::String(s)) => s,
        Some(other) => {
            return Err(error(400, "BadRequest", &format!("`backup` must be a string, not {}", other.type_name())));
        }
        None => return Err(error(400, "BadRequest", "give the backup's name in `backup`")),
    };
    let plain = !name.is_empty()
        && name != "."
        && name != ".."
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if !plain {
        return Err(error(
            400,
            "BadRequest",
            "a backup name may only hold letters, digits, `-`, `_` and `.`",
        ));
    }
    Ok(dir.join(name))
}

fn database_error(name: &str, e: &Error) -> Response {
    match e {
        Error::Runtime(m) if m.starts_with("database `") && m.ends_with("does not exist") => {
            error(404, "DatabaseNotFound", &format!("database `{name}` does not exist"))
        }
        _ => query_error(e),
    }
}

/// `POST /db/{name}/backup`: an online backup into the backup directory.
fn backup(&(ctx, me): &Req, name: &str, req: &Request) -> Response {
    if let Err(e) = check(me.access, Access::Admin, "backing up") {
        return query_error(&e);
    }
    let path = match body_fields(req, &["backup"]).and_then(|f| backup_path(ctx, &f)) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match ctx.mgr.backup_database(name, &path) {
        Ok(info) => {
            let mut body = String::from(r#"{"backup":{"database":"#);
            write_str(&mut body, &name.to_ascii_lowercase());
            body.push_str(r#","path":"#);
            write_str(&mut body, &info.dir.display().to_string());
            body.push_str(r#","format":"#);
            write_str(&mut body, info.format);
            let _ = write!(
                body,
                r#","nodes":{},"relationships":{},"bytes":{}}}}}"#,
                info.nodes, info.relationships, info.bytes
            );
            Response::json(200, body)
        }
        Err(e) => database_error(name, &e),
    }
}

/// `POST /db/{name}/restore`: creates (or with `"overwrite": true` replaces) a database
/// from a backup in the backup directory.
fn restore(&(ctx, me): &Req, name: &str, req: &Request) -> Response {
    if let Err(e) = check(me.access, Access::Admin, "restoring") {
        return query_error(&e);
    }
    let fields = match body_fields(req, &["backup", "overwrite"]) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let overwrite = match fields.get("overwrite") {
        None => false,
        Some(Json::Bool(b)) => *b,
        Some(other) => {
            return error(400, "BadRequest", &format!("`overwrite` must be a boolean, not {}", other.type_name()));
        }
    };
    let path = match backup_path(ctx, &fields) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !path.is_dir() {
        return error(404, "NotFound", "no backup with that name");
    }
    match ctx.mgr.restore_database(name, &path, overwrite) {
        Ok(info) => {
            let mut body = String::from(r#"{"database":{"name":"#);
            write_str(&mut body, &info.name);
            body.push_str(r#","storage":"#);
            write_str(&mut body, info.storage.name());
            let _ = write!(
                body,
                r#","nodes":{},"relationships":{}}}}}"#,
                info.nodes, info.relationships
            );
            Response::json(200, body)
        }
        Err(e) => query_error(&e),
    }
}

/// `POST /db/{name}/check`: an online consistency check of a persistent database.
fn check_database(&(ctx, me): &Req, name: &str) -> Response {
    if let Err(e) = check(me.access, Access::Admin, "checking a database") {
        return query_error(&e);
    }
    match ctx.mgr.check_database(name) {
        Ok(r) => {
            let mut body = String::from(r#"{"check":{"#);
            let _ = write!(
                body,
                r#""consistent":{},"nodes":{},"relationships":{},"indexEntries":{},"errorCount":{},"errors":["#,
                r.is_consistent(),
                r.nodes,
                r.relationships,
                r.index_entries,
                r.error_count
            );
            for (i, e) in r.errors.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                write_str(&mut body, e);
            }
            body.push_str(r#"],"warnings":["#);
            for (i, w) in r.warnings.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                write_str(&mut body, w);
            }
            body.push_str("]}}");
            Response::json(200, body)
        }
        Err(e) => database_error(name, &e),
    }
}

// ---- transactions -------------------------------------------------------------------------

/// The database a transaction route names (or the default one), lower-cased as the
/// catalog keys it.
fn tx_database(ctx: &Ctx, db: Option<&str>) -> String {
    db.map_or_else(|| ctx.mgr.default_database(), str::to_string)
        .to_ascii_lowercase()
}

/// `/db/{db}/tx/{id}`, percent-encoding the database name.
fn tx_path(db: &str, id: &str) -> String {
    let mut path = String::from("/db/");
    for b in db.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            path.push(b as char);
        } else {
            let _ = write!(path, "%{b:02X}");
        }
    }
    let _ = write!(path, "/tx/{id}");
    path
}

/// `POST /db/{name}/tx`: begins a transaction. The body may be empty or `{}`.
fn begin(&(ctx, me): &Req, db: Option<&str>, req: &Request) -> Response {
    if let Err(e) = check(me.access, Access::Read, "a transaction") {
        return query_error(&e);
    }
    let text = match json_body(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    if !text.trim().is_empty() {
        match json::parse(text) {
            Ok(Json::Object(m)) if m.is_empty() => {}
            Ok(Json::Object(m)) => {
                let k = m.keys().next().expect("not empty");
                return error(400, "BadRequest", &format!("unknown field `{k}`; begin takes no fields"));
            }
            Ok(_) => return error(400, "BadRequest", "the request body must be a JSON object"),
            Err(e) => return error(400, "BadRequest", &e.to_string()),
        }
    }
    let name = tx_database(ctx, db);
    let database = match ctx.mgr.database(&name) {
        Ok(d) => d,
        Err(_) => return error(404, "DatabaseNotFound", &format!("database `{name}` does not exist")),
    };
    match ctx.txs.begin(me.who, &name, database, ctx.query_timeout) {
        Ok(id) => {
            let path = tx_path(&name, &id);
            let mut body = String::from(r#"{"transaction":{"id":"#);
            write_str(&mut body, &id);
            body.push_str(r#","database":"#);
            write_str(&mut body, &name);
            body.push_str(r#","path":"#);
            write_str(&mut body, &path);
            let _ = write!(body, r#","timeoutMs":{}}}}}"#, ctx.txs.timeout().as_millis());
            Response::json(201, body).with_header("Location", path)
        }
        Err(e) => tx_error(e, false),
    }
}

/// `POST /db/{name}/tx/{id}` runs a statement; `DELETE` rolls back.
fn in_transaction(&(ctx, me): &Req, db: Option<&str>, id: &str, req: &Request) -> Response {
    let name = tx_database(ctx, db);
    match req.method.as_str() {
        "POST" => {}
        "DELETE" => {
            return match ctx.txs.rollback(me.who, &name, id) {
                Ok(()) => Response::json(200, r#"{"rolledBack":true}"#.into()),
                Err(e) => tx_error(e, true),
            };
        }
        _ => return not_allowed("POST, DELETE"),
    }
    let text = match json_body(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let (query, params, timeout) = match decode_request(text) {
        Ok(v) => v,
        Err(m) => return error(400, "BadRequest", &m),
    };
    // Statements rejected here leave the transaction open.
    let parsed = match transaction_query(&name, &query, &params) {
        Ok(q) => q,
        Err(e) => return query_error(&e),
    };
    let (needed, what) = authorize_query(&parsed);
    if let Err(e) = check(me.access, needed, what) {
        return query_error(&e);
    }
    let started = Instant::now();
    let result = ctx.txs.run(me.who, &name, id, parsed, params, ctx.query_options(timeout).timeout);
    let failure = match &result {
        Err(TxError::Failed(e)) => Some(e),
        _ => None,
    };
    ctx.finished("http", &name, me.who, &query, started.elapsed(), failure);
    match result {
        Ok(r) => Response::json(200, encode_result(&r)),
        Err(e) => tx_error(e, true),
    }
}

/// Parses a statement for a transaction on database `name`: a query, with no `USE` of
/// another database. `USE` alone and database commands are refused.
pub(crate) fn transaction_query(name: &str, text: &str, params: &BTreeMap<String, Value>) -> Result<Query, Error> {
    match graphbuff::cypher::parse_statement(text)? {
        Statement::Query { use_db, query } => {
            if let Some(other) = use_db {
                let other = match other {
                    DbName::Name(n) => Some(n),
                    DbName::Param(p) => match params.get(&p) {
                        Some(Value::String(s)) => Some(s.clone()),
                        _ => None,
                    },
                };
                if other.is_none_or(|o| !o.eq_ignore_ascii_case(name)) {
                    return Err(Error::Semantic(format!(
                        "this transaction runs on database `{name}`; USE cannot switch to another one"
                    )));
                }
            }
            Ok(query)
        }
        _ => Err(Error::Unsupported(
            "USE, database commands and user commands cannot run in a transaction".into(),
        )),
    }
}

/// `POST /db/{name}/tx/{id}/commit`.
fn commit(&(ctx, me): &Req, db: Option<&str>, id: &str) -> Response {
    match ctx.txs.commit(me.who, &tx_database(ctx, db), id) {
        Ok(()) => Response::json(200, r#"{"committed":true}"#.into()),
        Err(e) => tx_error(e, true),
    }
}

/// An error from a transaction route. With `rolled_back`, a failure means the open
/// transaction was rolled back, which the body says with `"rolledBack":true`.
fn tx_error(e: TxError, rolled_back: bool) -> Response {
    match e {
        TxError::NotFound => error(
            404,
            "TransactionNotFound",
            "no such open transaction: it has ended or timed out, or belongs to another database",
        ),
        TxError::Closed => error(503, "Busy", "the server is shutting down"),
        TxError::Failed(e) => {
            let mut resp = query_error(&e);
            if rolled_back {
                resp.body.pop(); // the closing brace
                resp.body.push_str(r#","rolledBack":true}"#);
            }
            resp
        }
    }
}

/// `{"query": "...", "params": {...}, "timeoutMs": n}`; `params` and `timeoutMs` may be
/// missing or null.
#[allow(clippy::type_complexity)]
fn decode_request(text: &str) -> Result<(String, BTreeMap<String, Value>, Option<Duration>), String> {
    let Json::Object(mut obj) = json::parse(text).map_err(|e| e.to_string())? else {
        return Err("the request body must be a JSON object".into());
    };
    let query = match obj.remove("query") {
        Some(Json::String(q)) => q,
        Some(other) => return Err(format!("`query` must be a string, got {}", other.type_name())),
        None => return Err("missing `query`".into()),
    };
    let params = match obj.remove("params") {
        None | Some(Json::Null) => BTreeMap::new(),
        Some(Json::Object(m)) => m.into_iter().map(|(k, v)| (k, to_value(v))).collect(),
        Some(other) => return Err(format!("`params` must be an object, got {}", other.type_name())),
    };
    let timeout = match obj.remove("timeoutMs") {
        None | Some(Json::Null) => None,
        Some(Json::Int(ms)) if ms > 0 => Some(Duration::from_millis(ms as u64)),
        Some(_) => return Err("`timeoutMs` must be a positive integer".into()),
    };
    if let Some(k) = obj.keys().next() {
        return Err(format!("unknown field `{k}`; expected `query`, `params` and `timeoutMs`"));
    }
    Ok((query, params, timeout))
}
