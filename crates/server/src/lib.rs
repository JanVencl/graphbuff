//! # graphbuff-server
//!
//! An HTTP/JSON server for graphbuff, so that programs in any language (Java, Python,
//! JavaScript, …) can run Cypher against a [`DatabaseManager`]. With
//! [`ServerConfig::bolt`] set it also speaks Bolt (4.4 and 5.0–5.4), the protocol of the
//! Neo4j drivers: auto-commit and explicit transactions, `PULL {n}`, `RESET`, and `ROUTE`
//! (this server is its own routing table, so `neo4j://` URIs work too). With a token,
//! Bolt clients send it as the password (`basic`, any user name) or as a `bearer` token.
//!
//! With [`ServerConfig::users`], clients log in as user accounts (HTTP Basic
//! authentication, Bolt `basic`) whose roles decide what they may run; see [`users`].
//! With [`ServerConfig::tls`] (and the `tls` or `tls-rustcrypto` feature) both protocols
//! run over TLS.
//!
//! ```text
//! GET  /health              {"status":"ok"} (no token needed)
//! GET  /db                  {"databases":[{"name","storage","default","nodes","relationships"}]}
//! POST /db/{name}/query     {"query":"MATCH …","params":{…}} against database `name`
//! POST /query               the same, against the default database
//!
//! POST   /db/{name}/tx              begin a transaction → 201 {"transaction":{"id",…}}
//! POST   /db/{name}/tx/{id}         {"query","params"}: run a statement in it
//! POST   /db/{name}/tx/{id}/commit  commit
//! DELETE /db/{name}/tx/{id}         roll back
//! /tx, /tx/{id}, …                  the same, against the default database
//! ```
//!
//! A query answers `200` with `{"columns":[…],"rows":[[…]],"stats":{…}}`, or an error
//! status with `{"error":{"code","message"}}`. A query request is its own transaction, as
//! with [`graphbuff::Database::execute`]. Database commands (`CREATE DATABASE`, `SHOW
//! DATABASES`, …) work through either query endpoint.
//!
//! An explicit transaction ([`graphbuff::Transaction`]) groups statements sent in
//! separate requests, on any connection. Its statements see each other's changes; other
//! clients see none of them until the commit. While it is open, other writers on that
//! database wait (including `begin`); readers do not. A statement that fails rolls the
//! whole transaction back (the error then has `"rolledBack":true`); one that does not
//! parse, or that is a `USE` or database command, is rejected and the transaction stays
//! open. A transaction with no request for [`ServerConfig::transaction_timeout`] is
//! rolled back, and later requests to it answer `404 TransactionNotFound`.
//!
//! A query (or a statement in a transaction) may run for at most
//! [`ServerConfig::query_timeout`], or less if its request asks with `"timeoutMs"`; one
//! that runs out of time is rolled back and answers `408 Timeout`. Rolling back a
//! transaction, and shutting down, cancel the statement it is running (`409 Cancelled`).
//!
//! The server uses only the standard library (and rustls for TLS): one thread per connection (two for Bolt), HTTP/1.1
//! keep-alive, `Content-Length` or chunked request bodies.
//!
//! ```no_run
//! use std::sync::Arc;
//! use graphbuff::DatabaseManager;
//! use graphbuff_server::{Server, ServerConfig};
//!
//! let mgr = Arc::new(DatabaseManager::open("data")?);
//! let server = Server::start(mgr, ServerConfig::default())?;
//! println!("listening on http://{}", server.local_addr());
//! server.wait();
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod api;
mod bolt;
mod crypto;
mod http;
pub mod log;
mod net;
pub mod packstream;
#[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
mod tls;
mod tx;
pub mod users;

/// The JSON reader and writer, and the JSON form of results and errors.
pub use graphbuff::json;
pub use log::{Level, Log, LogConfig};
pub use net::TlsConfig;
pub use users::{HashParams, Role, Users};

use std::io::{self, BufReader};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use graphbuff::{CancelHandle, DatabaseManager};

use api::Ctx;
use http::{Limits, ReadError};
use net::{Accepted, Stream, Tls};
use tx::Transactions;

/// How often blocked reads wake up to check for shutdown.
const POLL: Duration = Duration::from_millis(250);

/// Stack of the threads that run queries. Planning and running walk the query
/// recursively, so a long query (hundreds of `CREATE` clauses) needs more than the
/// default 2 MiB. Only the pages a thread touches are committed.
pub(crate) const QUERY_STACK: usize = 64 << 20;

/// Settings for [`Server::start`].
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Address to listen on. The default, `127.0.0.1:7475`, accepts local clients only.
    pub bind: String,
    /// When set, every request except `/health` needs `Authorization: Bearer <token>`
    /// (or Basic authentication with the token as the password); the token holder may
    /// do anything.
    pub token: Option<String>,
    /// User accounts. When set, clients log in as a user (HTTP Basic authentication,
    /// Bolt `basic` scheme) and may do what the user's roles allow; see [`users`].
    pub users: Option<Arc<Users>>,
    /// Connections served at once; more are answered with `503`.
    pub max_connections: usize,
    /// Largest accepted request body.
    pub max_body_bytes: usize,
    /// How long an idle keep-alive connection stays open.
    pub keep_alive: Duration,
    /// How long an explicit transaction may go without a request before it is rolled
    /// back. Other writers on its database wait meanwhile.
    pub transaction_timeout: Duration,
    /// The longest a query (or a statement in a transaction, or the wait to begin one)
    /// may run before it is stopped and rolled back. A request may ask for less with
    /// `timeoutMs`. `None` (the default): no limit but the request's own.
    pub query_timeout: Option<Duration>,
    /// Where to serve the Bolt protocol (Neo4j drivers), such as `127.0.0.1:7687`.
    /// `None` (the default): no Bolt.
    pub bolt: Option<String>,
    /// Serve HTTPS and Bolt over TLS, on the same ports. Needs the `tls` feature;
    /// without it [`Server::start`] fails when this is set. `None` (the default): no TLS.
    pub tls: Option<TlsConfig>,
    /// Where `POST /db/{name}/backup` writes backups and `POST /db/{name}/restore` reads
    /// them, one directory per backup. `None` (the default): both endpoints are off.
    pub backup_dir: Option<std::path::PathBuf>,
    /// Where events (failed logins, connections, errors) and slow queries are logged.
    /// `None` (the default): nowhere.
    pub log: Option<Arc<Log>>,
    /// At shutdown, how long queries already running may take to finish before they are
    /// cancelled (and rolled back). Default: 5 seconds.
    pub shutdown_grace: Duration,
    /// For compatibility tests (the openCypher TCK) only: turns on
    /// `POST /db/{name}/test-procedure`, which lets an admin add a procedure that answers
    /// from a fixed table. Default: off.
    pub test_procedures: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: "127.0.0.1:7475".into(),
            token: None,
            users: None,
            max_connections: 256,
            max_body_bytes: 16 << 20,
            keep_alive: Duration::from_secs(30),
            transaction_timeout: Duration::from_secs(60),
            query_timeout: None,
            bolt: None,
            tls: None,
            backup_dir: None,
            log: None,
            shutdown_grace: Duration::from_secs(5),
            test_procedures: false,
        }
    }
}

/// A running server. Dropping it (or calling [`shutdown`](Server::shutdown)) stops
/// accepting connections, rolls back open transactions, closes idle connections, and
/// gives queries in progress [`ServerConfig::shutdown_grace`] to finish before it cancels
/// them.
pub struct Server {
    addr: SocketAddr,
    bolt_addr: Option<SocketAddr>,
    stop: Arc<AtomicBool>,
    /// The accept threads, with the address each listens on.
    acceptors: Vec<(SocketAddr, JoinHandle<()>)>,
    conns: Arc<Mutex<Vec<JoinHandle<()>>>>,
    txs: Arc<Transactions>,
    abort: CancelHandle,
    grace: Duration,
}

struct Shared {
    ctx: Ctx,
    tls: Option<Tls>,
    limits: Limits,
    stop: Arc<AtomicBool>,
    active: AtomicUsize,
    max_connections: usize,
}

impl Shared {
    fn stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// The protocol a listener speaks.
#[derive(Clone, Copy)]
enum Protocol {
    Http,
    Bolt,
}

impl Server {
    /// Binds `config.bind` and starts serving `mgr` in background threads.
    pub fn start(mgr: Arc<DatabaseManager>, config: ServerConfig) -> io::Result<Server> {
        let tls = config.tls.as_ref().map(Tls::load).transpose()?;
        let listener = TcpListener::bind(&config.bind)?;
        let addr = listener.local_addr()?;
        let bolt = match &config.bolt {
            Some(b) => Some(TcpListener::bind(b)?),
            None => None,
        };
        let bolt_addr = bolt.as_ref().map(TcpListener::local_addr).transpose()?;
        let stop = Arc::new(AtomicBool::new(false));
        let conns: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let txs = Transactions::new(config.transaction_timeout);
        let abort = CancelHandle::new();
        let shared = Arc::new(Shared {
            ctx: Ctx {
                mgr,
                token: config.token,
                users: config.users,
                txs: Arc::clone(&txs),
                query_timeout: config.query_timeout,
                backup_dir: config.backup_dir,
                log: config.log,
                abort: abort.clone(),
                test_procedures: config.test_procedures,
            },
            tls,
            limits: Limits {
                max_body: config.max_body_bytes,
                idle: config.keep_alive,
                request: Duration::from_secs(30),
            },
            stop: Arc::clone(&stop),
            active: AtomicUsize::new(0),
            max_connections: config.max_connections.max(1),
        });
        let mut server = Server {
            addr,
            bolt_addr,
            stop,
            acceptors: Vec::new(),
            conns,
            txs,
            abort,
            grace: config.shutdown_grace,
        };
        let listeners = std::iter::once((listener, Protocol::Http)).chain(bolt.map(|b| (b, Protocol::Bolt)));
        for (listener, protocol) in listeners {
            let at = listener.local_addr()?;
            let (shared, conns) = (Arc::clone(&shared), Arc::clone(&server.conns));
            // On failure, dropping `server` stops the accept threads started so far.
            let h = thread::Builder::new()
                .name("graphbuff-accept".into())
                .spawn(move || accept_loop(listener, protocol, shared, conns))?;
            server.acceptors.push((at, h));
        }
        Ok(server)
    }

    /// The address the server listens on (useful with port `0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The address the Bolt protocol is served on, if [`ServerConfig::bolt`] is set.
    pub fn bolt_addr(&self) -> Option<SocketAddr> {
        self.bolt_addr
    }

    /// Blocks until the server stops (it runs until the process ends or another thread
    /// calls [`shutdown`](Server::shutdown) on it).
    pub fn wait(mut self) {
        for (_, h) in std::mem::take(&mut self.acceptors) {
            let _ = h.join();
        }
    }

    /// Stops the server and waits for its connections to finish.
    pub fn shutdown(mut self) {
        self.stop_now();
    }

    fn stop_now(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for (mut wake, h) in std::mem::take(&mut self.acceptors) {
            // Wake the acceptor, which is blocked in accept().
            if wake.ip().is_unspecified() {
                wake.set_ip(if wake.is_ipv4() {
                    std::net::Ipv4Addr::LOCALHOST.into()
                } else {
                    std::net::Ipv6Addr::LOCALHOST.into()
                });
            }
            let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
            let _ = h.join();
        }
        // Roll back open transactions first: a request waiting to begin one, or a
        // writing query, may be waiting for them to release their database.
        self.txs.close();
        let handles = std::mem::take(&mut *self.conns.lock().unwrap_or_else(|e| e.into_inner()));
        let deadline = Instant::now() + self.grace;
        while handles.iter().any(|h| !h.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        // Queries still running now are cancelled; their connections then close.
        self.abort.cancel();
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if !self.acceptors.is_empty() {
            self.stop_now();
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    protocol: Protocol,
    shared: Arc<Shared>,
    conns: Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let name = match protocol {
            Protocol::Http => "http",
            Protocol::Bolt => "bolt",
        };
        if shared.active.load(Ordering::SeqCst) >= shared.max_connections {
            if let Some(log) = &shared.ctx.log {
                log.warn(format_args!(
                    "{name} connection from {} refused: {} connections already open",
                    peer(&stream),
                    shared.max_connections
                ));
            }
            // A Bolt client sees the connection close during the handshake.
            if let Protocol::Http = protocol {
                let resp = api::error(503, "Busy", "too many connections; try again later");
                let mut stream = Stream::Plain(stream);
                let _ = http::write_response(&mut stream, &resp, false);
            }
            continue;
        }
        if let Some(log) = &shared.ctx.log {
            log.debug(format_args!("{name} connection from {}", peer(&stream)));
        }
        shared.active.fetch_add(1, Ordering::SeqCst);
        let s = Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name("graphbuff-conn".into())
            .stack_size(QUERY_STACK)
            .spawn(move || {
                if let Some(stream) = open(stream, protocol, &s) {
                    match protocol {
                        Protocol::Http => serve(stream, &s),
                        Protocol::Bolt => bolt::serve(stream, &s),
                    }
                }
                s.active.fetch_sub(1, Ordering::SeqCst);
            });
        let mut list = conns.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|h| !h.is_finished());
        match spawned {
            Ok(h) => list.push(h),
            Err(_) => {
                shared.active.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

/// The client's address, for the log.
fn peer(stream: &TcpStream) -> String {
    stream.peer_addr().map_or_else(|_| "unknown".into(), |a| a.to_string())
}

/// Sets up a new connection: timeouts, then TLS if the server has it.
fn open(tcp: TcpStream, protocol: Protocol, shared: &Shared) -> Option<Stream> {
    let _ = tcp.set_nodelay(true);
    if tcp.set_read_timeout(Some(POLL)).is_err() || tcp.set_write_timeout(Some(Duration::from_secs(30))).is_err() {
        return None;
    }
    match net::accept(tcp, shared.tls.as_ref(), shared.limits.request, &|| shared.stop()) {
        Accepted::Stream(s) => Some(s),
        Accepted::PlainRefused(tcp) => {
            if let Protocol::Http = protocol {
                let mut reader = BufReader::new(Stream::Plain(tcp.try_clone().ok()?));
                let resp = api::error(400, "BadRequest", "this server only accepts HTTPS");
                let _ = http::write_response(&mut Stream::Plain(tcp), &resp, false);
                http::drain(&mut reader);
            }
            None
        }
        Accepted::Gone => None,
    }
}

fn serve(stream: Stream, shared: &Shared) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let peer = stream.peer_addr().map_or_else(|_| "unknown".into(), |a| a.to_string());
    let mut reader = BufReader::new(stream);
    let stop = || shared.stop.load(Ordering::SeqCst);
    loop {
        match http::read_request(&mut reader, &mut writer, &shared.limits, &stop) {
            Ok(None) => return,
            Ok(Some(req)) => {
                let resp = api::handle(&shared.ctx, &req, &peer);
                let keep = req.keep_alive && !stop();
                if http::write_response(&mut writer, &resp, keep).is_err() || !keep {
                    return;
                }
            }
            Err(ReadError::Bad(status, msg)) => {
                let resp = api::error(status, "BadRequest", &msg);
                let _ = http::write_response(&mut writer, &resp, false);
                http::drain(&mut reader);
                return;
            }
            Err(ReadError::Io) => return,
        }
    }
}
