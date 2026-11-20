//! The transactions open on a server and the statements they run, for
//! `SHOW TRANSACTIONS` and `TERMINATE TRANSACTIONS`.
//!
//! Every query run on its own (over HTTP or Bolt) is a transaction for as long as it
//! runs; an explicit transaction is one from its begin to its commit or rollback, and
//! shows the statement it is running, if any. Each gets an id `DATABASE-transaction-N`.
//!
//! Terminating one stops it: a query run on its own is cancelled (and rolled back); an
//! explicit transaction is rolled back, cancelling its statement if it runs one.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use graphbuff::CancelHandle;

use crate::users::Principal;

/// How a transaction is stopped.
#[derive(Clone)]
pub(crate) enum Stop {
    /// Cancel this handle: a query run on its own, or a Bolt transaction (whose
    /// connection rolls it back when it sees the handle cancelled).
    Cancel(CancelHandle),
    /// An HTTP transaction, by its id in [`crate::tx::Transactions`].
    Http(String),
}

/// What a client sees of one transaction.
pub(crate) struct Info {
    pub database: String,
    pub who: Principal,
    pub protocol: &'static str,
    pub connection: String,
    pub client: String,
}

struct Entry {
    info: Info,
    id: String,
    started: SystemTime,
    started_at: Instant,
    /// The statement it runs: query id, text, start.
    query: Option<(String, String, SystemTime, Instant)>,
    stop: Stop,
}

/// One row of `SHOW TRANSACTIONS`.
pub(crate) struct Row {
    pub database: String,
    pub transaction_id: String,
    pub current_query_id: String,
    pub connection_id: String,
    pub client_address: String,
    pub username: String,
    pub current_query: String,
    pub start_ms: i64,
    pub status: &'static str,
    pub elapsed_ms: i64,
    pub protocol: &'static str,
    /// The current query's start and run time, if it runs one.
    pub query_start_ms: Option<i64>,
    pub query_elapsed_ms: Option<i64>,
    pub who: Principal,
}

/// The open transactions of a server.
#[derive(Default)]
pub(crate) struct Running {
    next: AtomicU64,
    queries: AtomicU64,
    entries: Mutex<BTreeMap<u64, Entry>>,
}

/// Unlists its transaction when dropped.
pub(crate) struct Guard {
    running: Arc<Running>,
    key: u64,
}

impl Guard {
    /// Shows `text` as the statement the transaction runs (`None`: none).
    pub(crate) fn query(&self, text: Option<&str>) {
        self.running.set_query(self.key, text);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.running.lock().remove(&self.key);
    }
}

fn millis(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// The user name shown for `who`.
pub(crate) fn username(who: &Principal) -> String {
    match who {
        Principal::Open => String::new(),
        Principal::Token => "token".into(),
        Principal::User(name) => name.clone(),
    }
}

impl Running {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<u64, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lists a transaction until the returned guard is dropped. `query`: the statement it
    /// runs from the start (a query run on its own).
    pub(crate) fn begin(self: &Arc<Self>, info: Info, stop: Stop, query: Option<&str>) -> Guard {
        let key = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let entry = Entry {
            id: format!("{}-transaction-{key}", info.database),
            info,
            started: SystemTime::now(),
            started_at: Instant::now(),
            query: None,
            stop,
        };
        self.lock().insert(key, entry);
        let guard = Guard { running: Arc::clone(self), key };
        if query.is_some() {
            guard.query(query);
        }
        guard
    }

    fn set_query(&self, key: u64, text: Option<&str>) {
        let query = text.map(|t| {
            let n = self.queries.fetch_add(1, Ordering::Relaxed) + 1;
            (format!("query-{n}"), t.to_string(), SystemTime::now(), Instant::now())
        });
        if let Some(e) = self.lock().get_mut(&key) {
            e.query = query;
        }
    }

    /// The transactions, in the order they began.
    pub(crate) fn rows(&self) -> Vec<Row> {
        let now = Instant::now();
        self.lock()
            .values()
            .map(|e| Row {
                database: e.info.database.clone(),
                transaction_id: e.id.clone(),
                current_query_id: e.query.as_ref().map_or(String::new(), |q| q.0.clone()),
                connection_id: e.info.connection.clone(),
                client_address: e.info.client.clone(),
                username: username(&e.info.who),
                current_query: e.query.as_ref().map_or(String::new(), |q| q.1.clone()),
                start_ms: millis(e.started),
                status: if e.query.is_some() { "Running" } else { "Idle" },
                elapsed_ms: now.duration_since(e.started_at).as_millis() as i64,
                protocol: e.info.protocol,
                query_start_ms: e.query.as_ref().map(|q| millis(q.2)),
                query_elapsed_ms: e.query.as_ref().map(|q| now.duration_since(q.3).as_millis() as i64),
                who: e.info.who.clone(),
            })
            .collect()
    }

    /// How to stop transaction `id`, and who it belongs to; `None` if there is none.
    pub(crate) fn find(&self, id: &str) -> Option<(Stop, Principal)> {
        self.lock().values().find(|e| e.id == id).map(|e| (e.stop.clone(), e.info.who.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(db: &str) -> Info {
        Info {
            database: db.into(),
            who: Principal::User("ann".into()),
            protocol: "http",
            connection: "http".into(),
            client: "127.0.0.1:5000".into(),
        }
    }

    #[test]
    fn lists_until_dropped() {
        let running = Arc::new(Running::default());
        let a = running.begin(info("graphbuff"), Stop::Cancel(CancelHandle::new()), Some("RETURN 1"));
        let b = running.begin(info("movies"), Stop::Http("x".into()), None);
        let rows = running.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].transaction_id, "graphbuff-transaction-1");
        assert_eq!((rows[0].status, rows[0].current_query.as_str()), ("Running", "RETURN 1"));
        assert!(rows[0].current_query_id.starts_with("query-"));
        assert_eq!((rows[1].status, rows[1].current_query.as_str()), ("Idle", ""));
        b.query(Some("MATCH (n) RETURN n"));
        assert_eq!(running.rows()[1].status, "Running");
        b.query(None);
        assert_eq!(running.rows()[1].current_query_id, "");
        assert!(running.find("movies-transaction-2").is_some());
        drop(a);
        assert_eq!(running.rows().len(), 1);
        assert!(running.find("graphbuff-transaction-1").is_none());
    }
}

// ---- SHOW / TERMINATE TRANSACTIONS ----------------------------------------------------------

use graphbuff::cypher::ast::{
    Clause, Expr, Projection, ProjectionItem, Query, QueryMode, ShowTail, SingleQuery, TransactionCommand,
};
use graphbuff::{Database, Error, QueryResult, Value};

use crate::api::Ctx;
use crate::users::Access;

/// The parameter that carries the rows into the query that applies `YIELD` and the rest.
const ROWS: &str = "__graphbuff_transaction_rows";

/// The columns of `SHOW TRANSACTIONS`: how each is computed from a row (`r`, a list), and
/// whether it is shown without `YIELD`.
const SHOW_COLUMNS: &[(&str, &str, bool)] = &[
    ("database", "r[0]", true),
    ("transactionId", "r[1]", true),
    ("currentQueryId", "r[2]", true),
    ("connectionId", "r[3]", true),
    ("clientAddress", "r[4]", true),
    ("username", "r[5]", true),
    ("currentQuery", "r[6]", true),
    ("startTime", "toString(datetime.fromEpochMillis(r[7]))", true),
    ("status", "r[8]", true),
    ("elapsedTime", "duration({milliseconds: r[9]})", true),
    ("protocol", "r[10]", false),
    (
        "currentQueryStartTime",
        "CASE WHEN r[11] IS NULL THEN null ELSE toString(datetime.fromEpochMillis(r[11])) END",
        false,
    ),
    ("currentQueryElapsedTime", "CASE WHEN r[12] IS NULL THEN null ELSE duration({milliseconds: r[12]}) END", false),
];

/// The columns of `TERMINATE TRANSACTIONS`.
const TERMINATE_COLUMNS: &[(&str, &str, bool)] =
    &[("transactionId", "r[0]", true), ("username", "r[1]", true), ("message", "r[2]", true)];

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// The transaction ids `ids` name: strings, or parameters holding a string or a list of
/// them.
fn ids(ids: &[Expr], params: &BTreeMap<String, Value>) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    let mut add = |v: &Value| match v {
        Value::String(s) => {
            out.push(s.clone());
            Ok(())
        }
        Value::List(items) => items.iter().try_for_each(|i| match i {
            Value::String(s) => {
                out.push(s.clone());
                Ok(())
            }
            o => Err(Error::Type(format!("a transaction id is a STRING, not {}", o.type_name()))),
        }),
        o => Err(Error::Type(format!("a transaction id is a STRING, not {}", o.type_name()))),
    };
    for e in ids {
        match e {
            Expr::Literal(v) => add(v)?,
            Expr::Param(p) => add(params.get(p).ok_or_else(|| Error::Semantic(format!("parameter ${p} is missing")))?)?,
            _ => return Err(Error::Semantic("give transaction ids as strings or parameters".into())),
        }
    }
    Ok(out)
}

/// Runs `UNWIND rows WITH <columns> <tail>` on an empty database: the rows as the
/// command's columns, then the command's `YIELD`, `WHERE` and `RETURN`.
fn project(
    columns: &[(&str, &str, bool)],
    rows: Vec<Value>,
    tail: &ShowTail,
    params: &BTreeMap<String, Value>,
) -> Result<QueryResult, Error> {
    let with: Vec<String> = columns.iter().map(|(name, expr, _)| format!("{expr} AS `{name}`")).collect();
    let text = format!("UNWIND ${ROWS} AS r WITH {}", with.join(", "));
    let mut query = graphbuff::cypher::parse(&text)?;
    let clauses = &mut query.parts[0].clauses;
    let var = |name: &str| ProjectionItem { expr: Expr::Var(name.to_string()), alias: name.to_string() };
    let shown = |names: Vec<&str>| Projection {
        distinct: false,
        star: false,
        items: names.into_iter().map(var).collect(),
        order_by: Vec::new(),
        skip: None,
        limit: None,
        where_: None,
    };
    let defaults = || shown(columns.iter().filter(|c| c.2).map(|c| c.0).collect());
    match tail {
        ShowTail { yields: Some(y), returns, .. } => {
            clauses.push(Clause::With(y.clone()));
            let returns = returns.clone().unwrap_or_else(|| {
                let mut names: Vec<&str> = if y.star { columns.iter().map(|c| c.0).collect() } else { Vec::new() };
                names.extend(y.items.iter().map(|i| i.alias.as_str()));
                shown(names)
            });
            clauses.push(Clause::Return(returns));
        }
        ShowTail { where_: Some(w), .. } => {
            let mut filter = shown(columns.iter().map(|c| c.0).collect());
            filter.where_ = Some(w.clone());
            clauses.push(Clause::With(filter));
            clauses.push(Clause::Return(defaults()));
        }
        _ => clauses.push(Clause::Return(defaults())),
    }
    let query = Query { parts: vec![SingleQuery { clauses: query.parts.remove(0).clauses }], union_all: false, mode: QueryMode::Run };
    let mut params = params.clone();
    params.insert(ROWS.into(), Value::List(rows));
    Database::in_memory().execute_query(&query, &params)
}

impl Ctx {
    /// Whether `who` (who may do `access`) sees the transactions of `owner`: their own,
    /// or everyone's for administrators.
    fn sees(who: &Principal, access: Access, owner: &Principal) -> bool {
        access == Access::Admin || matches!(who, Principal::Open | Principal::Token) || who == owner
    }

    /// Runs `SHOW TRANSACTIONS` or `TERMINATE TRANSACTIONS` for `who`.
    pub(crate) fn transaction_command(
        &self,
        who: &Principal,
        access: Access,
        cmd: &TransactionCommand,
        params: &BTreeMap<String, Value>,
    ) -> Result<QueryResult, Error> {
        let wanted = ids(&cmd.ids, params)?;
        if cmd.terminate {
            let mut rows = Vec::new();
            for id in &wanted {
                let found = self.running.find(id).filter(|(_, owner)| Self::sees(who, access, owner));
                let (user, message) = match found {
                    Some((stop, owner)) => {
                        match stop {
                            Stop::Cancel(handle) => handle.cancel(),
                            Stop::Http(tx) => {
                                self.txs.terminate(&tx);
                            }
                        }
                        if let Some(log) = &self.log {
                            log.info(format_args!("transaction {id} terminated by `{}`", username(who)));
                        }
                        (s(&username(&owner)), "Transaction terminated.")
                    }
                    None => (Value::Null, "Transaction not found."),
                };
                rows.push(Value::List(vec![s(id), user, s(message)]));
            }
            return project(TERMINATE_COLUMNS, rows, &cmd.tail, params);
        }
        let rows = self
            .running
            .rows()
            .into_iter()
            .filter(|r| Self::sees(who, access, &r.who))
            .filter(|r| wanted.is_empty() || wanted.contains(&r.transaction_id))
            .map(|r| {
                Value::List(vec![
                    s(&r.database),
                    s(&r.transaction_id),
                    s(&r.current_query_id),
                    s(&r.connection_id),
                    s(&r.client_address),
                    s(&r.username),
                    s(&r.current_query),
                    Value::Int(r.start_ms),
                    s(r.status),
                    Value::Int(r.elapsed_ms),
                    s(r.protocol),
                    r.query_start_ms.map_or(Value::Null, Value::Int),
                    r.query_elapsed_ms.map_or(Value::Null, Value::Int),
                ])
            })
            .collect();
        project(SHOW_COLUMNS, rows, &cmd.tail, params)
    }
}

#[cfg(test)]
mod show_tests {
    use super::*;

    fn tail(src: &str) -> ShowTail {
        match graphbuff::cypher::parse_statement(src).unwrap() {
            graphbuff::cypher::ast::Statement::Transaction(c) => c.tail,
            o => panic!("{o:?}"),
        }
    }

    fn row(id: &str, user: &str, elapsed: i64) -> Value {
        Value::List(vec![
            s("graphbuff"),
            s(id),
            s(""),
            s("http"),
            s("127.0.0.1:1"),
            s(user),
            s(""),
            Value::Int(1_700_000_000_000),
            s("Idle"),
            Value::Int(elapsed),
            s("http"),
            Value::Null,
            Value::Null,
        ])
    }

    #[test]
    fn yield_where_and_return() {
        let rows = || vec![row("a", "ann", 20_000), row("b", "bob", 5)];
        let p = BTreeMap::new();
        let all = project(SHOW_COLUMNS, rows(), &ShowTail::default(), &p).unwrap();
        assert_eq!(all.columns.len(), 10);
        assert_eq!(all.rows.len(), 2);
        assert_eq!(all.rows[0][7].to_string(), "\"2023-11-14T22:13:20Z\"");
        let slow = tail("SHOW TRANSACTIONS YIELD transactionId, elapsedTime WHERE elapsedTime.milliseconds > 10000 RETURN transactionId");
        let r = project(SHOW_COLUMNS, rows(), &slow, &p).unwrap();
        assert_eq!((r.columns.clone(), r.rows.len()), (vec!["transactionId".to_string()], 1));
        let r = project(SHOW_COLUMNS, rows(), &tail("SHOW TRANSACTIONS WHERE username = 'bob'"), &p).unwrap();
        assert_eq!(r.rows.len(), 1);
        let r = project(SHOW_COLUMNS, rows(), &tail("SHOW TRANSACTIONS YIELD *"), &p).unwrap();
        assert_eq!(r.columns.len(), SHOW_COLUMNS.len());
        let r = project(SHOW_COLUMNS, rows(), &tail("SHOW TRANSACTIONS YIELD username AS u ORDER BY u DESC"), &p).unwrap();
        assert_eq!(r.columns, ["u"]);
        assert_eq!(r.rows[0][0].to_string(), "\"bob\"");
    }
}
