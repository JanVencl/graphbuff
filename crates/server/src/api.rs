//! The JSON API: routes, request decoding and result encoding.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

use graphbuff::{
    DatabaseManager, Error, NodeData, PathData, PlanDescription, Point, QueryOptions, QueryResult, QueryStats, RelData,
    ResultValue, Value,
};

use graphbuff::cypher::ast::{DatabaseCommand, DbName, Query, QueryMode, Statement};

use crate::crypto;
use crate::http::{Request, Response};
use crate::json::{self, Json, write_float, write_str};
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
}

impl Ctx {
    /// The limits for a query that asked for `requested` milliseconds (if anything).
    pub(crate) fn query_options(&self, requested: Option<Duration>) -> QueryOptions {
        QueryOptions {
            timeout: match (requested, self.query_timeout) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            cancel: None,
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

pub(crate) fn handle(ctx: &Ctx, req: &Request) -> Response {
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
    let mut body = String::from(r#"{"error":{"code":"#);
    write_str(&mut body, code);
    body.push_str(r#","message":"#);
    write_str(&mut body, message);
    body.push_str("}}");
    Response::json(status, body)
}

fn not_allowed(allow: &'static str) -> Response {
    error(405, "MethodNotAllowed", &format!("use {allow}")).with_header("Allow", allow)
}

/// Maps an engine error to a status code and an error object.
fn query_error(e: &Error) -> Response {
    let (status, code, message) = match e {
        Error::Syntax { message, offset } => {
            let mut body = String::from(r#"{"error":{"code":"SyntaxError","message":"#);
            write_str(&mut body, message);
            let _ = write!(body, r#","offset":{offset}}}}}"#);
            return Response::json(400, body);
        }
        Error::Semantic(m) => (400, "SemanticError", m),
        Error::Type(m) => (400, "TypeError", m),
        Error::Runtime(m) => (400, "RuntimeError", m),
        Error::Unsupported(m) => (400, "Unsupported", m),
        Error::Constraint(m) => (409, "ConstraintError", m),
        Error::Storage(m) => (500, "StorageError", m),
        Error::TransactionTooLarge(m) => (413, "TransactionTooLarge", m),
        Error::Timeout(m) => (408, "Timeout", m),
        Error::Cancelled(m) => (409, "Cancelled", m),
        Error::Forbidden(m) => (403, "Forbidden", m),
    };
    error(status, code, message)
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
    match session.execute_with_options(&query, &params, &ctx.query_options(timeout)) {
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
    let query = match transaction_query(&name, &query, &params) {
        Ok(q) => q,
        Err(e) => return query_error(&e),
    };
    let (needed, what) = authorize_query(&query);
    if let Err(e) = check(me.access, needed, what) {
        return query_error(&e);
    }
    match ctx.txs.run(me.who, &name, id, query, params, ctx.query_options(timeout).timeout) {
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

fn to_value(j: Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(b),
        Json::Int(i) => Value::Int(i),
        Json::Float(f) => Value::Float(f),
        Json::String(s) => Value::String(s),
        Json::Array(items) => Value::List(items.into_iter().map(to_value).collect()),
        Json::Object(m) => Value::Map(m.into_iter().map(|(k, v)| (k, to_value(v))).collect()),
    }
}

// ---- results ------------------------------------------------------------------------------

fn encode_result(r: &QueryResult) -> String {
    let mut out = String::with_capacity(64 + r.rows.len() * 32);
    out.push_str(r#"{"columns":["#);
    for (i, c) in r.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_str(&mut out, c);
    }
    out.push_str(r#"],"rows":["#);
    for (i, row) in r.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, v) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            write_result_value(&mut out, v);
        }
        out.push(']');
    }
    out.push_str(r#"],"stats":"#);
    write_stats(&mut out, &r.stats);
    if let Some(plan) = &r.plan {
        out.push_str(r#","plan":"#);
        write_plan(&mut out, plan);
    }
    out.push('}');
    out
}

/// `{"profiled": bool, "lines": [{"depth", "operator", "estimatedRows", "rows", "timeMs"}]}`;
/// numbers that were not estimated or measured are `null`.
fn write_plan(out: &mut String, plan: &PlanDescription) {
    let _ = write!(out, r#"{{"profiled":{},"lines":["#, plan.profiled);
    let num = |out: &mut String, v: Option<f64>| match v {
        Some(v) if v.is_finite() => write_float(out, v),
        _ => out.push_str("null"),
    };
    for (i, l) in plan.lines.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, r#"{{"depth":{},"operator":"#, l.depth);
        write_str(out, &l.operator);
        out.push_str(r#","estimatedRows":"#);
        num(out, l.estimated_rows);
        out.push_str(r#","rows":"#);
        match l.rows {
            Some(r) => {
                let _ = write!(out, "{r}");
            }
            None => out.push_str("null"),
        }
        out.push_str(r#","timeMs":"#);
        num(out, l.time.map(|t| t.as_secs_f64() * 1e3));
        out.push('}');
    }
    out.push_str("]}");
}

fn write_stats(out: &mut String, s: &QueryStats) {
    let _ = write!(
        out,
        concat!(
            r#"{{"nodesCreated":{},"nodesDeleted":{},"relationshipsCreated":{},"#,
            r#""relationshipsDeleted":{},"propertiesSet":{},"labelsAdded":{},"#,
            r#""labelsRemoved":{},"indexesAdded":{},"indexesRemoved":{},"#,
            r#""constraintsAdded":{},"constraintsRemoved":{},"#,
            r#""systemUpdates":{},"containsUpdates":{}}}"#
        ),
        s.nodes_created,
        s.nodes_deleted,
        s.relationships_created,
        s.relationships_deleted,
        s.properties_set,
        s.labels_added,
        s.labels_removed,
        s.indexes_added,
        s.indexes_removed,
        s.constraints_added,
        s.constraints_removed,
        s.system_updates,
        s.contains_updates()
    );
}

/// Property values (scalars and lists) as plain JSON.
fn write_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(f) => write_float(out, *f),
        Value::String(s) => write_str(out, s),
        Value::List(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, x);
            }
            out.push(']');
        }
        Value::Map(m) => write_props(out, m),
        // Not storable as properties; never part of a materialized result's properties.
        Value::Node(id) => {
            let _ = write!(out, r#"{{"$type":"node","id":{id}}}"#);
        }
        Value::Rel(id) => {
            let _ = write!(out, r#"{{"$type":"relationship","id":{id}}}"#);
        }
        Value::Path(_) => out.push_str("null"),
        Value::Temporal(t) => write_str(out, &t.to_string()),
        Value::Point(p) => write_point(out, p),
    }
}

fn write_props(out: &mut String, m: &BTreeMap<String, Value>) {
    out.push('{');
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_str(out, k);
        out.push(':');
        write_value(out, v);
    }
    out.push('}');
}

fn write_node(out: &mut String, n: &NodeData) {
    let _ = write!(out, r#"{{"$type":"node","id":{},"labels":["#, n.id);
    for (i, l) in n.labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_str(out, l);
    }
    out.push_str(r#"],"properties":"#);
    write_props(out, &n.properties);
    out.push('}');
}

fn write_rel(out: &mut String, r: &RelData) {
    let _ = write!(out, r#"{{"$type":"relationship","id":{},"type":"#, r.id);
    write_str(out, &r.rel_type);
    let _ = write!(out, r#","start":{},"end":{},"properties":"#, r.start, r.end);
    write_props(out, &r.properties);
    out.push('}');
}

fn write_path(out: &mut String, p: &PathData) {
    out.push_str(r#"{"$type":"path","nodes":["#);
    for (i, n) in p.nodes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_node(out, n);
    }
    out.push_str(r#"],"relationships":["#);
    for (i, r) in p.relationships.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_rel(out, r);
    }
    out.push_str("]}");
}

fn write_result_value(out: &mut String, v: &ResultValue) {
    match v {
        ResultValue::Null => out.push_str("null"),
        ResultValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        ResultValue::Int(i) => {
            let _ = write!(out, "{i}");
        }
        ResultValue::Float(f) => write_float(out, *f),
        ResultValue::String(s) => write_str(out, s),
        ResultValue::List(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_result_value(out, x);
            }
            out.push(']');
        }
        ResultValue::Map(m) => {
            out.push('{');
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(out, k);
                out.push(':');
                write_result_value(out, v);
            }
            out.push('}');
        }
        ResultValue::Node(n) => write_node(out, n),
        ResultValue::Relationship(r) => write_rel(out, r),
        ResultValue::Path(p) => write_path(out, p),
        ResultValue::Temporal(t) => write_str(out, &t.to_string()),
        ResultValue::Point(p) => write_point(out, p),
    }
}

/// A point as `{"$type":"point","crs":"cartesian","srid":7203,"x":1.0,"y":2.0}` (and `"z"`).
fn write_point(out: &mut String, p: &Point) {
    out.push_str(r#"{"$type":"point","crs":"#);
    write_str(out, p.crs().name());
    let _ = write!(out, r#","srid":{}"#, p.crs().srid());
    for (name, c) in ["x", "y", "z"].iter().zip(p.coords()) {
        let _ = write!(out, r#","{name}":"#);
        write_float(out, *c);
    }
    out.push('}');
}
