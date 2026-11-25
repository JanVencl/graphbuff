//! The Bolt protocol (versions 4.4, 5.0–5.4 and 5.6–5.8), spoken by the Neo4j drivers.
//!
//! A connection starts with a handshake: the magic `60 60 B0 17` and four version
//! proposals, answered with the chosen version; or (5.7+) a manifest request
//! `00 00 01 FF`, answered with the versions served, from which the client picks one.
//! Bolt 5.5 was never served by Neo4j and is skipped. Then messages flow, each a PackStream
//! structure split into chunks (a big-endian `u16` size, the bytes, and a `00 00` end).
//!
//! Every connection has two threads. A reader decodes messages and queues them, so that
//! a `RESET` can interrupt the statement that is running (it cancels its
//! [`CancelHandle`]); messages queued before the `RESET` are then answered `IGNORED`. The
//! connection's own thread runs the messages in order and writes the answers.
//!
//! An auto-commit `RUN` of a read-only query streams: the query runs while `PULL`s take
//! its rows, `n` at a time, and waits for the next `PULL` when `n` are sent, so a result
//! is never held in memory. Other results (in an explicit transaction, or of a query
//! that writes) are computed by `RUN` and kept until `PULL` or `DISCARD` take them. An
//! explicit transaction (`BEGIN` … `COMMIT`) is owned by the connection's
//! thread, which holds the database's write lock until it ends; it is rolled back when
//! no message arrives for [`crate::ServerConfig::transaction_timeout`], or when the
//! connection closes.

use std::collections::BTreeMap;
use std::io::{self, BufWriter, ErrorKind, Read, Write};
use std::net::SocketAddr;

use crate::net::Stream;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use graphbuff::cypher::ast::{Query, Statement, UserCommand};

use crate::users::{Access, Principal};
use crate::running::{Guard, Info, Stop};
use graphbuff::temporal::{self, NANOS_PER_DAY, NANOS_PER_SECOND};
use graphbuff::{
    BoxedStore, CancelHandle, Crs, Error, NodeData, PathData, PlanDescription, Point, QueryOptions, QueryResult,
    QueryStats, RelData, ResultValue, RowSink, Temporal, Transaction, Value,
};

use crate::packstream::{self, PackValue};
use crate::{POLL, Shared, api};

const MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];
/// The newest Bolt 5 minor version served.
const MAX_V5_MINOR: u8 = 8;
/// The Bolt 5 minor version no server negotiates (its design was withdrawn).
const SKIPPED_V5_MINOR: u8 = 5;
/// A handshake slot asking for the manifest (version 1) instead of a version.
const MANIFEST_V1: [u8; 4] = [0, 0, 1, 0xFF];
/// The versions a manifest lists, as `[_, range, minor, major]`: 5.8–5.6, 5.4–5.0, 4.4.
const MANIFEST_VERSIONS: [[u8; 4]; 3] = [[0, 2, 8, 5], [0, 4, 4, 5], [0, 0, 4, 4]];

// Requests.
const HELLO: u8 = 0x01;
const GOODBYE: u8 = 0x02;
const RESET: u8 = 0x0F;
const RUN: u8 = 0x10;
const BEGIN: u8 = 0x11;
const COMMIT: u8 = 0x12;
const ROLLBACK: u8 = 0x13;
const DISCARD: u8 = 0x2F;
const PULL: u8 = 0x3F;
const TELEMETRY: u8 = 0x54;
const ROUTE: u8 = 0x66;
const LOGON: u8 = 0x6A;
const LOGOFF: u8 = 0x6B;

// Responses.
const SUCCESS: u8 = 0x70;
const RECORD: u8 = 0x71;
const IGNORED: u8 = 0x7E;
const FAILURE: u8 = 0x7F;

// Value structures.
const NODE: u8 = 0x4E;
const RELATIONSHIP: u8 = 0x52;
const UNBOUND_RELATIONSHIP: u8 = 0x72;
const PATH: u8 = 0x50;
const DATE: u8 = 0x44;
const TIME: u8 = 0x54;
const LOCAL_TIME: u8 = 0x74;
const DATE_TIME: u8 = 0x49;
const DATE_TIME_LEGACY: u8 = 0x46;
const DATE_TIME_ZONE_ID: u8 = 0x69;
const DATE_TIME_ZONE_ID_LEGACY: u8 = 0x66;
const LOCAL_DATE_TIME: u8 = 0x64;
const DURATION: u8 = 0x45;
const POINT_2D: u8 = 0x58;
const POINT_3D: u8 = 0x59;

// Error codes, in Neo4j's `Neo.<classification>.<category>.<title>` form.
const INVALID: &str = "Neo.ClientError.Request.Invalid";
const UNAUTHORIZED: &str = "Neo.ClientError.Security.Unauthorized";
const RATE_LIMIT: &str = "Neo.ClientError.Security.AuthenticationRateLimit";
const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";
const TYPE_ERROR: &str = "Neo.ClientError.Statement.TypeError";
const NOT_SUPPORTED: &str = "Neo.ClientError.Statement.FeatureNotSupported";
const DATABASE_NOT_FOUND: &str = "Neo.ClientError.Database.DatabaseNotFound";
const TIMED_OUT: &str = "Neo.ClientError.Transaction.TransactionTimedOut";

/// The name drivers use for administration commands; with no real database of that
/// name, it runs database commands only.
const SYSTEM_DB: &str = "system";

/// How long a closing connection waits for the client to close its side.
const LINGER: Duration = Duration::from_secs(1);

/// Numbers the connections (for `connection_id`) and the bookmarks.
static CONNECTIONS: AtomicU64 = AtomicU64::new(0);
static BOOKMARKS: AtomicU64 = AtomicU64::new(0);

/// What the client's proposals ask for: a version, or the manifest.
#[derive(Debug, PartialEq, Eq)]
enum Handshake {
    Version(u8, u8),
    Manifest,
}

/// The newest version served of the 5.x range `lowest..=minor`.
fn best_v5(lowest: u8, minor: u8) -> Option<u8> {
    let mut best = minor.min(MAX_V5_MINOR);
    if best == SKIPPED_V5_MINOR {
        best -= 1;
    }
    (best >= lowest).then_some(best)
}

/// What a client and the server agree on, from the client's four proposals (in its order
/// of preference).
fn negotiate(proposals: &[u8]) -> Option<Handshake> {
    for slot in proposals.as_chunks::<4>().0 {
        if *slot == MANIFEST_V1 {
            return Some(Handshake::Manifest);
        }
        let [_, range, minor, major] = *slot;
        let lowest = minor.saturating_sub(range);
        match major {
            5 => {
                if let Some(m) = best_v5(lowest, minor) {
                    return Some(Handshake::Version(5, m));
                }
            }
            4 if minor >= 4 && lowest <= 4 => return Some(Handshake::Version(4, 4)),
            _ => {}
        }
    }
    None
}

/// Whether `(major, minor)` is a version served.
fn served(major: u8, minor: u8) -> bool {
    matches!((major, minor), (4, 4)) || (major == 5 && minor <= MAX_V5_MINOR && minor != SKIPPED_V5_MINOR)
}

/// The server's manifest: `00 00 01 FF`, the number of version ranges (a varint), the
/// ranges, and the capabilities (a varint; none).
fn manifest() -> Vec<u8> {
    let mut out = MANIFEST_V1.to_vec();
    out.push(MANIFEST_VERSIONS.len() as u8);
    for v in MANIFEST_VERSIONS {
        out.extend(v);
    }
    out.push(0);
    out
}

/// Reads a varint (7 bits per byte, least significant first, high bit set on all but
/// the last byte).
fn read_varint(stream: &mut Stream, keep_going: &dyn Fn() -> bool) -> io::Result<Option<u64>> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let mut b = [0u8];
        if !read_full(stream, &mut b, keep_going)? {
            return Ok(None);
        }
        value |= u64::from(b[0] & 0x7F) << shift;
        if b[0] & 0x80 == 0 {
            return Ok(Some(value));
        }
    }
    Err(io::Error::new(ErrorKind::InvalidData, "varint too long"))
}

/// Reads exactly `buf.len()` bytes. Read timeouts are retried while `keep_going` says
/// so; `Ok(false)` means it said stop.
fn read_full(stream: &mut Stream, buf: &mut [u8], keep_going: &dyn Fn() -> bool) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if !keep_going() {
                    return Ok(false);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Serves one Bolt connection until it closes or the server stops.
pub(crate) fn serve(mut stream: Stream, shared: &Shared) {
    let started = Instant::now();
    let mut hello = [0u8; 20];
    let in_time = || !shared.stop() && started.elapsed() < shared.limits.request;
    if !matches!(read_full(&mut stream, &mut hello, &in_time), Ok(true)) || hello[..4] != MAGIC {
        return;
    }
    let (version, by_manifest) = match negotiate(&hello[4..]) {
        None => {
            let _ = stream.write_all(&[0, 0, 0, 0]);
            return;
        }
        Some(Handshake::Version(major, minor)) => {
            if stream.write_all(&[0, 0, minor, major]).is_err() {
                return;
            }
            ((major, minor), false)
        }
        Some(Handshake::Manifest) => {
            if stream.write_all(&manifest()).is_err() || stream.flush().is_err() {
                return;
            }
            // The client answers with the version it picked and the capabilities it uses.
            let mut chosen = [0u8; 4];
            if !matches!(read_full(&mut stream, &mut chosen, &in_time), Ok(true))
                || !matches!(read_varint(&mut stream, &in_time), Ok(Some(_)))
            {
                return;
            }
            let [_, _, minor, major] = chosen;
            if !served(major, minor) {
                return;
            }
            ((major, minor), true)
        }
    };
    let (Ok(reader), Ok(writer), Ok(local)) = (stream.try_clone(), stream.try_clone(), stream.local_addr()) else {
        return;
    };
    let peer = stream.peer_addr().map_or_else(|_| "unknown".into(), |a| a.to_string());
    let control = Control::default();
    let (queue, inbox) = mpsc::channel();
    thread::scope(|s| {
        let control = &control;
        let read = thread::Builder::new()
            .name("graphbuff-bolt-read".into())
            .spawn_scoped(s, move || read_messages(reader, queue, control, shared));
        if read.is_err() {
            return;
        }
        let mut conn = Conn {
            shared,
            out: BufWriter::new(writer),
            inbox,
            control,
            local,
            peer,
            version,
            by_manifest,
            utc: version.0 >= 5,
            hello: false,
            who: None,
            failed: false,
            pending_failure: None,
            connection: "bolt".into(),
            tx_stop: None,
            stashed: None,
        };
        let _ = conn.run();
        let _ = conn.out.flush();
        // The reader stops at its next message (it has no one to send it to), when the
        // client closes, or when the final shutdown wakes it.
        drop(conn);
        stream.close_gracefully(LINGER, &|| !shared.stop());
    });
}

/// Runs `f` so that a `RESET` (or the client leaving, or shutdown, through `base`) can
/// cancel it. `None`: a `RESET` came first, or cancelled it; the message is then ignored.
fn interruptible<T>(
    control: &Control,
    base: &CancelHandle,
    max_memory: Option<u64>,
    timeout: Option<Duration>,
    f: impl FnOnce(&QueryOptions) -> graphbuff::Result<T>,
) -> Option<graphbuff::Result<T>> {
    let cancel = base.child();
    *control.running() = Some(cancel.clone());
    // Checked after publishing the handle: a RESET read from now on cancels it.
    let result = if control.reset_pending() {
        None
    } else {
        Some(f(&QueryOptions {
            timeout,
            cancel: Some(cancel),
            max_memory,
        }))
    };
    *control.running() = None;
    match result {
        Some(Err(Error::Cancelled(_))) if control.reset_pending() => None,
        r => r,
    }
}

// ---- reading ------------------------------------------------------------------------------

/// A message from the reader thread.
enum Incoming {
    Message { tag: u8, fields: Vec<PackValue> },
    /// The client broke the protocol; the connection closes after answering.
    Bad(String),
}

/// What the reader shares with the connection's thread, for `RESET`.
#[derive(Default)]
struct Control {
    /// `RESET`s read but not yet handled.
    resets: AtomicUsize,
    /// Cancels the statement (or wait to begin) running now.
    running: Mutex<Option<CancelHandle>>,
}

impl Control {
    fn running(&self) -> std::sync::MutexGuard<'_, Option<CancelHandle>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn interrupt(&self) {
        if let Some(c) = self.running().as_ref() {
            c.cancel();
        }
    }

    fn reset_pending(&self) -> bool {
        self.resets.load(Ordering::SeqCst) > 0
    }
}

fn read_messages(mut stream: Stream, queue: Sender<Incoming>, control: &Control, shared: &Shared) {
    let keep_going = || !shared.stop();
    let mut message = Vec::new();
    loop {
        let mut size = [0u8; 2];
        if !matches!(read_full(&mut stream, &mut size, &keep_going), Ok(true)) {
            break;
        }
        let n = u16::from_be_bytes(size) as usize;
        if n == 0 {
            if message.is_empty() {
                continue; // a no-op chunk, sent to keep the connection alive
            }
            let incoming = match packstream::decode(&message) {
                Ok(PackValue::Struct { tag, fields }) => {
                    if tag == RESET {
                        control.resets.fetch_add(1, Ordering::SeqCst);
                        control.interrupt();
                    }
                    Incoming::Message { tag, fields }
                }
                Ok(other) => Incoming::Bad(format!("a message must be a structure, not a {}", other.type_name())),
                Err(e) => Incoming::Bad(format!("malformed message: {e}")),
            };
            message.clear();
            let bad = matches!(incoming, Incoming::Bad(_));
            if queue.send(incoming).is_err() || bad {
                break;
            }
            continue;
        }
        if message.len() + n > shared.limits.max_body {
            let _ = queue.send(Incoming::Bad(format!(
                "a message is larger than {} bytes",
                shared.limits.max_body
            )));
            break;
        }
        let start = message.len();
        message.resize(start + n, 0);
        if !matches!(read_full(&mut stream, &mut message[start..], &keep_going), Ok(true)) {
            break;
        }
    }
    // The client left: stop what runs for it. When the server is stopping instead, the
    // statement may finish within the shutdown grace period (it is cancelled after).
    if !shared.stop() {
        control.interrupt();
    }
}

// ---- the connection -----------------------------------------------------------------------

/// A failure to report: a Neo4j error code and a message.
struct Failure {
    code: &'static str,
    message: String,
}

/// The GQL status and its description for a Neo4j error code (Bolt 5.7+), by the
/// code's category: GQL's general class codes where Neo4j has no closer one.
fn gql_status(code: &str) -> (&'static str, &'static str) {
    let category = code.split('.').nth(2).unwrap_or("");
    let title = code.split('.').nth(3).unwrap_or("");
    match (category, title) {
        ("Statement", "SyntaxError") => ("42001", "error: syntax error or access rule violation - invalid syntax"),
        ("Statement", "SemanticError" | "FeatureNotSupported") | ("Security", _) | ("Database", "DatabaseNotFound") => {
            ("42000", "error: syntax error or access rule violation")
        }
        ("Statement", _) | ("Schema", _) | ("Request", _) => ("22000", "error: data exception"),
        ("Transaction", _) => ("40000", "error: transaction rollback"),
        _ => ("50N42", "error: general processing exception - unexpected error"),
    }
}

/// The GQL classification of a Neo4j error code: `CLIENT_ERROR`, `TRANSIENT_ERROR` or
/// `DATABASE_ERROR`.
fn classification(code: &str) -> &'static str {
    match code.split('.').nth(1) {
        Some("ClientError") => "CLIENT_ERROR",
        Some("TransientError") => "TRANSIENT_ERROR",
        _ => "DATABASE_ERROR",
    }
}

/// A GQL diagnostic record with the default operation and schema, and `extra` entries.
fn diagnostic_record(extra: &[(&str, &str)]) -> PackValue {
    let mut m = BTreeMap::from([
        ("OPERATION".to_string(), PackValue::str("")),
        ("OPERATION_CODE".to_string(), PackValue::str("0")),
        ("CURRENT_SCHEMA".to_string(), PackValue::str("/")),
    ]);
    for (k, v) in extra {
        m.insert(k.to_string(), PackValue::str(*v));
    }
    PackValue::Map(m)
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Failure {
        Failure {
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Failure {
        Failure::new(INVALID, message)
    }

    fn from_error(e: &Error) -> Failure {
        let code = match e {
            Error::Syntax { .. } => "Neo.ClientError.Statement.SyntaxError",
            Error::Semantic(_) => "Neo.ClientError.Statement.SemanticError",
            Error::Type(_) => TYPE_ERROR,
            Error::Runtime(_) => "Neo.ClientError.Statement.ArgumentError",
            Error::Unsupported(_) => NOT_SUPPORTED,
            Error::Constraint(_) => "Neo.ClientError.Schema.ConstraintValidationFailed",
            Error::Storage(_) => "Neo.DatabaseError.General.UnknownError",
            Error::TransactionTooLarge(_) => "Neo.ClientError.Transaction.TransactionTooLarge",
            Error::Timeout(_) => TIMED_OUT,
            Error::Cancelled(_) => "Neo.ClientError.Transaction.Terminated",
            Error::MemoryLimit(_) => "Neo.TransientError.General.MemoryPoolOutOfMemoryError",
            Error::Forbidden(_) => FORBIDDEN,
        };
        Failure::new(code, e.to_string())
    }
}

/// What a message did to the connection.
enum Flow {
    Continue,
    Close,
}

/// What the connection's thread waits for next.
enum Event {
    Message(u8, Vec<PackValue>),
    /// No message before the deadline.
    Idle,
    /// The open transaction was terminated (`TERMINATE TRANSACTIONS`).
    Terminated,
    /// The client left or broke the protocol, or the server is stopping.
    Close,
}

/// A result computed by `RUN`, waiting for `PULL` or `DISCARD`.
struct Pending {
    rows: std::vec::IntoIter<Vec<ResultValue>>,
    /// Whether the result has columns, and rows (for the GQL status, 5.6+).
    has_columns: bool,
    has_rows: bool,
    stats: QueryStats,
    plan: Option<PlanDescription>,
    db: String,
    started: Instant,
}

impl Pending {
    fn new(r: QueryResult, db: String, started: Instant) -> (Pending, Vec<String>) {
        let pending = Pending {
            has_columns: !r.columns.is_empty(),
            has_rows: !r.rows.is_empty(),
            rows: r.rows.into_iter(),
            stats: r.stats,
            plan: r.plan,
            db,
            started,
        };
        (pending, r.columns)
    }
}

struct Conn<'s> {
    shared: &'s Shared,
    out: BufWriter<Stream>,
    inbox: Receiver<Incoming>,
    control: &'s Control,
    local: SocketAddr,
    /// The client's address, for the log.
    peer: String,
    version: (u8, u8),
    /// The version was picked from the manifest (HELLO then reports it).
    by_manifest: bool,
    /// Whether date-times travel as UTC instants (Bolt 5, or the 4.4 `utc` patch).
    utc: bool,
    hello: bool,
    /// Who logged in; `None` before that.
    who: Option<Principal>,
    /// After a failure, messages are ignored until `RESET`.
    failed: bool,
    /// A failure to report on the next message (a transaction that timed out).
    pending_failure: Option<Failure>,
    /// The connection's id (`bolt-N`), set by `HELLO`.
    connection: String,
    /// While an explicit transaction is open: cancelled by `TERMINATE TRANSACTIONS`.
    tx_stop: Option<CancelHandle>,
    /// A message read while streaming that ended the stream, handled next.
    stashed: Option<(u8, Fields)>,
}

type Fields = Vec<PackValue>;
type Params = BTreeMap<String, Value>;
type Extra = BTreeMap<String, PackValue>;

impl Conn<'_> {
    fn at_least(&self, major: u8, minor: u8) -> bool {
        self.version >= (major, minor)
    }

    // ---- writing ----

    fn send(&mut self, tag: u8, fields: Fields) -> io::Result<()> {
        let mut buf = Vec::new();
        packstream::encode(&mut buf, &PackValue::Struct { tag, fields });
        for chunk in buf.chunks(u16::MAX as usize) {
            self.out.write_all(&(chunk.len() as u16).to_be_bytes())?;
            self.out.write_all(chunk)?;
        }
        self.out.write_all(&[0, 0])
    }

    fn success(&mut self, meta: BTreeMap<String, PackValue>) -> io::Result<()> {
        self.send(SUCCESS, vec![PackValue::Map(meta)])
    }

    fn ignored(&mut self) -> io::Result<()> {
        self.send(IGNORED, vec![])
    }

    /// Reports a failure; later messages are ignored until `RESET`. From 5.7 it is a GQL
    /// error: `neo4j_code` (for `code`), `gql_status`, `description` and
    /// `diagnostic_record`.
    fn fail(&mut self, f: Failure) -> io::Result<()> {
        self.failed = true;
        let meta = if self.at_least(5, 7) {
            let (status, description) = gql_status(f.code);
            PackValue::map([
                ("neo4j_code", PackValue::str(f.code)),
                ("message", PackValue::String(f.message)),
                ("gql_status", PackValue::str(status)),
                ("description", PackValue::str(description)),
                ("diagnostic_record", diagnostic_record(&[("_classification", classification(f.code))])),
            ])
        } else {
            PackValue::map([("code", PackValue::str(f.code)), ("message", PackValue::String(f.message))])
        };
        self.send(FAILURE, vec![meta])
    }

    // ---- reading ----

    /// The next message, flushing the answers written so far before waiting.
    fn next(&mut self, deadline: Option<Instant>) -> io::Result<Event> {
        if let Some((tag, fields)) = self.stashed.take() {
            return Ok(Event::Message(tag, fields));
        }
        let terminated = |c: &Self| c.tx_stop.as_ref().is_some_and(CancelHandle::is_cancelled);
        if terminated(self) {
            return Ok(Event::Terminated);
        }
        let incoming = match self.inbox.try_recv() {
            Ok(m) => m,
            Err(TryRecvError::Disconnected) => return Ok(Event::Close),
            Err(TryRecvError::Empty) => {
                self.out.flush()?;
                loop {
                    if self.shared.stop() {
                        return Ok(Event::Close);
                    }
                    if terminated(self) {
                        return Ok(Event::Terminated);
                    }
                    let mut wait = POLL;
                    if let Some(d) = deadline {
                        let left = d.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return Ok(Event::Idle);
                        }
                        wait = wait.min(left);
                    }
                    match self.inbox.recv_timeout(wait) {
                        Ok(m) => break m,
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return Ok(Event::Close),
                    }
                }
            }
        };
        if self.shared.stop() {
            return Ok(Event::Close);
        }
        match incoming {
            Incoming::Message { tag, fields } => Ok(Event::Message(tag, fields)),
            Incoming::Bad(message) => {
                self.fail(Failure::invalid(message))?;
                Ok(Event::Close)
            }
        }
    }

    /// Handles a `RESET`: everything before it is forgotten.
    fn reset(&mut self) -> io::Result<()> {
        self.control.resets.fetch_sub(1, Ordering::SeqCst);
        self.failed = false;
        self.pending_failure = None;
        self.success(BTreeMap::new())
    }

    /// Runs `f` so that a `RESET` (or the client leaving, or shutdown) can cancel it.
    /// `None`: a `RESET` came first, or cancelled it; the message is then ignored.
    fn interruptible<T>(
        &self,
        timeout: Option<Duration>,
        f: impl FnOnce(&QueryOptions) -> graphbuff::Result<T>,
    ) -> Option<graphbuff::Result<T>> {
        let base = self.tx_stop.as_ref().unwrap_or(&self.shared.ctx.abort);
        interruptible(self.control, base, self.shared.ctx.query_max_memory, timeout, f)
    }

    // ---- the message loop ----

    fn run(&mut self) -> io::Result<()> {
        let mut streaming: Option<Pending> = None;
        loop {
            let (tag, fields) = match self.next(None)? {
                Event::Message(t, f) => (t, f),
                Event::Idle | Event::Close | Event::Terminated => return Ok(()),
            };
            match tag {
                GOODBYE => return Ok(()),
                RESET => {
                    streaming = None;
                    self.reset()?;
                    continue;
                }
                _ if self.control.reset_pending() => {
                    self.ignored()?;
                    continue;
                }
                _ => {}
            }
            if !self.hello {
                if tag != HELLO {
                    self.fail(Failure::invalid("the first message must be HELLO"))?;
                    return Ok(());
                }
                match self.hello(fields)? {
                    Flow::Continue => continue,
                    Flow::Close => return Ok(()),
                }
            }
            if self.failed {
                self.ignored()?;
                continue;
            }
            if let Some(f) = self.pending_failure.take() {
                self.fail(f)?;
                continue;
            }
            if self.who.is_none() {
                if tag == LOGON {
                    match self.logon(fields)? {
                        Flow::Continue => continue,
                        Flow::Close => return Ok(()),
                    }
                }
                self.fail(Failure::new(UNAUTHORIZED, "authenticate with LOGON first"))?;
                return Ok(());
            }
            match tag {
                RUN if streaming.is_none() => streaming = self.run_autocommit(fields)?,
                PULL | DISCARD if streaming.is_some() => {
                    let p = streaming.as_mut().expect("checked");
                    if self.stream(p, tag == PULL, &fields, true)? {
                        streaming = None;
                    }
                }
                BEGIN if streaming.is_none() => {
                    if let Flow::Close = self.begin(fields)? {
                        return Ok(());
                    }
                }
                ROUTE => self.route(fields)?,
                TELEMETRY if self.at_least(5, 4) => self.success(BTreeMap::new())?,
                LOGOFF if self.at_least(5, 1) => {
                    streaming = None;
                    self.who = None;
                    self.success(BTreeMap::new())?;
                }
                RUN | BEGIN => self.fail(Failure::invalid("a result is still open: PULL or DISCARD it first"))?,
                PULL | DISCARD => self.fail(Failure::invalid("there is no result to PULL or DISCARD"))?,
                COMMIT | ROLLBACK => self.fail(Failure::invalid("there is no open transaction"))?,
                HELLO | LOGON => self.fail(Failure::invalid("already authenticated"))?,
                other => self.fail(Failure::invalid(format!("unknown message 0x{other:02X}")))?,
            }
        }
    }

    // ---- authentication ----

    /// Who `auth` (`scheme`, `principal`, `credentials`) logs in as: `basic` with a
    /// user's name and password, or with the server token as the password (any name);
    /// `bearer` with the token.
    /// Fails with the failure to send: wrong credentials, or locked out after too many.
    fn login(&self, auth: &BTreeMap<String, PackValue>) -> Result<Principal, Failure> {
        let ctx = &self.shared.ctx;
        let text = |key: &str| match auth.get(key) {
            Some(PackValue::String(s)) => s.as_str(),
            _ => "",
        };
        let scheme = text("scheme");
        let user = match scheme {
            "basic" => text("principal").to_string(),
            "bearer" => crate::users::bearer_name(text("credentials")),
            _ => String::new(),
        };
        let counts = matches!(scheme, "basic" | "bearer");
        let attempt = || match scheme {
            "basic" => ctx.login(text("principal"), text("credentials")),
            "bearer" => ctx.bearer_login(text("credentials")),
            _ => None,
        };
        let who = ctx.throttled_login("bolt", &self.peer, &user, counts, attempt).map_err(|e| match e {
            api::LoginError::Wrong => Failure::new(UNAUTHORIZED, "wrong or missing credentials"),
            api::LoginError::Locked(wait) => Failure::new(RATE_LIMIT, api::LoginError::locked_message(wait)),
        })?;
        // An expired API token.
        match ctx.access(&who) {
            Some(_) => Ok(who),
            None => Err(Failure::new(UNAUTHORIZED, api::no_access(&who))),
        }
    }

    /// What the logged-in client may do now.
    fn access(&self) -> Result<(&Principal, Access), Failure> {
        let who = self.who.as_ref().ok_or_else(|| Failure::new(UNAUTHORIZED, "not logged in"))?;
        match self.shared.ctx.access(who) {
            Some(a) => Ok((who, a)),
            None => Err(Failure::new(UNAUTHORIZED, api::no_access(who))),
        }
    }

    fn hello(&mut self, fields: Fields) -> io::Result<Flow> {
        let Some(PackValue::Map(extra)) = fields.into_iter().next() else {
            self.fail(Failure::invalid("HELLO takes a map"))?;
            return Ok(Flow::Close);
        };
        // From 5.1 the credentials come with LOGON.
        if !self.at_least(5, 1) {
            match self.login(&extra) {
                Ok(who) => self.who = Some(who),
                Err(failure) => {
                    self.fail(failure)?;
                    return Ok(Flow::Close);
                }
            }
        }
        self.hello = true;
        let id = CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        let mut meta = BTreeMap::new();
        // Drivers expect a server agent of the form `Neo4j/x.y.z`.
        meta.insert(
            "server".into(),
            PackValue::String(format!(
                "Neo4j/{}.{}.0-graphbuff-{}",
                self.version.0,
                self.version.1,
                env!("CARGO_PKG_VERSION")
            )),
        );
        self.connection = format!("bolt-{id}");
        meta.insert("connection_id".into(), PackValue::String(self.connection.clone()));
        if self.by_manifest {
            meta.insert(
                "protocol_version".into(),
                PackValue::String(format!("{}.{}", self.version.0, self.version.1)),
            );
        }
        if self.version == (4, 4)
            && let Some(PackValue::List(patches)) = extra.get("patch_bolt")
            && patches.contains(&PackValue::str("utc"))
        {
            self.utc = true;
            meta.insert("patch_bolt".into(), PackValue::List(vec![PackValue::str("utc")]));
        }
        self.success(meta)?;
        Ok(Flow::Continue)
    }

    fn logon(&mut self, fields: Fields) -> io::Result<Flow> {
        let Some(PackValue::Map(auth)) = fields.into_iter().next() else {
            self.fail(Failure::invalid("LOGON takes a map"))?;
            return Ok(Flow::Close);
        };
        match self.login(&auth) {
            Ok(who) => self.who = Some(who),
            Err(failure) => {
                self.fail(failure)?;
                return Ok(Flow::Close);
            }
        }
        self.success(BTreeMap::new())?;
        Ok(Flow::Continue)
    }

    // ---- statements ----

    /// The database named by an `extra` map's `db` (the default one when missing), and
    /// whether it exists.
    fn database_name(&self, extra: &BTreeMap<String, PackValue>) -> Result<String, Failure> {
        match extra.get("db") {
            None | Some(PackValue::Null) => Ok(self.shared.ctx.mgr.default_database()),
            Some(PackValue::String(s)) if s.is_empty() => Ok(self.shared.ctx.mgr.default_database()),
            Some(PackValue::String(s)) => match self.shared.ctx.mgr.database(s) {
                Ok(_) => Ok(s.to_ascii_lowercase()),
                Err(_) if s.eq_ignore_ascii_case(SYSTEM_DB) => Ok(SYSTEM_DB.to_string()),
                Err(_) => Err(Failure::new(DATABASE_NOT_FOUND, format!("database `{s}` does not exist"))),
            },
            Some(other) => Err(Failure::invalid(format!("`db` must be a string, got {}", other.type_name()))),
        }
    }

    /// Whether `name` (from [`Self::database_name`]) is the `system` database, which
    /// cannot be queried, so it only runs database and user commands.
    fn is_system(&self, name: &str) -> bool {
        name == SYSTEM_DB
    }

    /// Checks the parts of an `extra` map that are not supported, and returns the time
    /// limit it asks for (`tx_timeout`, in milliseconds), capped by the server's.
    fn time_limit(&self, extra: &BTreeMap<String, PackValue>) -> Result<Option<Duration>, Failure> {
        if let Some(PackValue::String(_)) = extra.get("imp_user") {
            return Err(Failure::new(NOT_SUPPORTED, "impersonation (imp_user) is not supported"));
        }
        let requested = match extra.get("tx_timeout") {
            None | Some(PackValue::Null) => None,
            Some(PackValue::Int(ms)) if *ms > 0 => Some(Duration::from_millis(*ms as u64)),
            // Zero (or less) means the server's limit, as in Neo4j.
            Some(PackValue::Int(_)) => None,
            Some(other) => {
                return Err(Failure::invalid(format!(
                    "`tx_timeout` must be an integer, got {}",
                    other.type_name()
                )));
            }
        };
        Ok(self.shared.ctx.query_options(requested).timeout)
    }

    /// `RUN`'s fields: the query, its parameters and the extra map.
    fn run_fields(fields: Fields) -> Result<(String, Params, Extra), Failure> {
        let mut it = fields.into_iter();
        let (Some(PackValue::String(query)), params, extra) = (it.next(), it.next(), it.next()) else {
            return Err(Failure::invalid("RUN takes a query string, a parameter map and a map"));
        };
        let params = match params {
            None | Some(PackValue::Null) => BTreeMap::new(),
            Some(PackValue::Map(m)) => m
                .into_iter()
                .map(|(k, v)| Ok((k, to_value(v)?)))
                .collect::<Result<_, Failure>>()?,
            Some(other) => {
                return Err(Failure::invalid(format!(
                    "the parameters must be a map, got {}",
                    other.type_name()
                )));
            }
        };
        let extra = match extra {
            None | Some(PackValue::Null) => BTreeMap::new(),
            Some(PackValue::Map(m)) => m,
            Some(other) => return Err(Failure::invalid(format!("RUN's extra must be a map, got {}", other.type_name()))),
        };
        Ok((query, params, extra))
    }

    /// A `RUN` outside a transaction: the query is its own transaction. A read-only query
    /// streams its rows to the `PULL`s that follow (see [`Streamer`]); any other result
    /// is returned, to be pulled.
    fn run_autocommit(&mut self, fields: Fields) -> io::Result<Option<Pending>> {
        let started = Instant::now();
        let prepared = Self::run_fields(fields).and_then(|(query, params, extra)| {
            let db = self.database_name(&extra)?;
            let timeout = self.time_limit(&extra)?;
            Ok((query, params, db, timeout, no_db(&extra)))
        });
        let (query, params, db, timeout, home) = match prepared {
            Ok(v) => v,
            Err(f) => {
                self.fail(f)?;
                return Ok(None);
            }
        };
        let (who, access) = match self.access() {
            Ok((w, a)) => (w.clone(), a),
            Err(f) => {
                self.fail(f)?;
                return Ok(None);
            }
        };
        let shared = self.shared;
        let ctx = &shared.ctx;
        let system = self.is_system(&db);
        let (connection, peer) = (self.connection.clone(), self.peer.clone());
        let base = self.tx_stop.clone().unwrap_or_else(|| ctx.abort.clone());
        let control = self.control;
        // The home database it ran on, for the driver's cache (5.8+).
        let resolved = (home && self.at_least(5, 8)).then(|| db.clone());
        let audit_peer = self.peer.clone();
        let mut action = None;
        let mut streamer = Streamer {
            conn: self,
            started,
            resolved,
            deadline: timeout.map(|t| started + t),
            columns: Vec::new(),
            opened: false,
            request: None,
            ended: None,
        };
        let result = interruptible(control, &base, ctx.query_max_memory, timeout, |options| {
            let statement = graphbuff::cypher::parse_statement(&query)?;
            action = api::audit_action(&statement, &params);
            if let Statement::User(cmd) = &statement {
                return ctx.user_command(&who, access, cmd, &params)?.feed(&mut streamer);
            }
            if let Statement::Transaction(cmd) = &statement {
                return ctx.transaction_command(&who, access, cmd, &params)?.feed(&mut streamer);
            }
            if let Statement::Settings(cmd) = &statement {
                return ctx.settings_command(access, cmd, &params)?.feed(&mut streamer);
            }
            let info = Info { database: db.clone(), who: who.clone(), protocol: "bolt", connection, client: peer };
            let stop = Stop::Cancel(options.cancel.clone().expect("interruptible sets one"));
            let _listed = ctx.running.begin(info, stop, Some(&query));
            ctx.authorize(&who, &statement, &db, &params)?;
            let mut session = ctx.mgr.session();
            if system {
                system_command(&statement)?;
            } else {
                session.use_database(&db)?;
            }
            session.execute_streaming(&query, &params, options, &mut streamer)
        });
        let Streamer {
            opened,
            ended,
            resolved,
            ..
        } = streamer;
        let failure = match (&result, &ended) {
            // Stopped by the stream itself: not the query's failure.
            (_, Some(_)) => None,
            (Some(Err(e)), None) => Some(e),
            _ => None,
        };
        if result.is_some() {
            ctx.finished("bolt", &db, &who, &query, started.elapsed(), failure);
        }
        if let (Some(a), Some(r)) = (&action, &result) {
            ctx.audit("bolt", &audit_peer, &who, a, r.as_ref().err().map(|e| e as &dyn std::fmt::Display));
        }
        if !opened {
            // No row came: answer the RUN, as for a result computed by it.
            return match (result, ended) {
                (_, Some(Ended::Io(e))) => Err(e),
                (None, _) => {
                    self.ignored()?;
                    Ok(None)
                }
                (Some(Err(e)), _) => {
                    self.fail(Failure::from_error(&e))?;
                    Ok(None)
                }
                (Some(Ok(r)), _) => {
                    let (pending, columns) = Pending::new(r, db, started);
                    let mut meta = run_success(columns, started, None);
                    if let Some(db) = resolved {
                        meta.insert("db".into(), PackValue::String(db));
                    }
                    self.success(meta)?;
                    Ok(Some(pending))
                }
            };
        }
        let discarded = match ended {
            Some(Ended::Io(e)) => return Err(e),
            // Answered already, or the client is gone.
            Some(Ended::Answered | Ended::Close) => return Ok(None),
            Some(Ended::Stashed) => {
                // Only a RESET, GOODBYE or LOGOFF ends a result early (and after a RESET,
                // the messages before it are ignored).
                if let Some((tag, _)) = &self.stashed
                    && !matches!(*tag, RESET | GOODBYE | LOGOFF)
                    && !control.reset_pending()
                {
                    self.stashed = None;
                    self.fail(Failure::invalid("a result is still open: PULL or DISCARD it first"))?;
                }
                return Ok(None);
            }
            Some(Ended::Discarded) => true,
            None => false,
        };
        // Answer the PULL or DISCARD that took the last rows.
        let (stats, plan) = match result {
            None => return self.ignored().map(|()| None),
            Some(Ok(r)) => (r.stats, r.plan),
            Some(Err(_)) if discarded => (QueryStats::default(), None),
            Some(Err(e)) => return self.fail(Failure::from_error(&e)).map(|()| None),
        };
        let p = Pending {
            rows: Vec::new().into_iter(),
            has_columns: true,
            has_rows: true,
            stats,
            plan,
            db,
            started,
        };
        let meta = self.summary(&p, true);
        self.success(meta)?;
        Ok(None)
    }

    /// `PULL` (or `DISCARD`, without sending the rows) `n` rows of `p`. Returns whether
    /// the result is used up.
    fn stream(
        &mut self,
        p: &mut Pending,
        send_rows: bool,
        fields: &[PackValue],
        autocommit: bool,
    ) -> io::Result<bool> {
        let n = match pull_size(fields) {
            Ok(n) => n,
            Err(f) => {
                self.fail(f)?;
                return Ok(false);
            }
        };
        let enc = Encoder {
            element_ids: self.version.0 >= 5,
            utc: self.utc,
        };
        for row in p.rows.by_ref().take(n) {
            if send_rows {
                let values = row.iter().map(|v| enc.result(v)).collect();
                self.send(RECORD, vec![PackValue::List(values)])?;
            }
        }
        if p.rows.len() > 0 {
            self.success(BTreeMap::from([("has_more".into(), PackValue::Bool(true))]))?;
            return Ok(false);
        }
        let meta = self.summary(p, autocommit);
        self.success(meta)?;
        Ok(true)
    }

    /// The `SUCCESS` that ends a result: its type, timing, database, statistics, plan and
    /// (5.6+) GQL status; a bookmark when it ends an auto-commit transaction.
    fn summary(&self, p: &Pending, autocommit: bool) -> BTreeMap<String, PackValue> {
        let mut meta = BTreeMap::new();
        if autocommit {
            meta.insert("bookmark".into(), PackValue::String(new_bookmark()));
        }
        // Neo4j's query types: "s" for schema or system changes alone.
        let s = &p.stats;
        let schema = s.system_updates + s.indexes_added + s.indexes_removed + s.constraints_added + s.constraints_removed;
        let data = s.nodes_created
            + s.nodes_deleted
            + s.relationships_created
            + s.relationships_deleted
            + s.properties_set
            + s.labels_added
            + s.labels_removed;
        let kind = if schema > 0 && data == 0 {
            "s"
        } else if p.stats.contains_updates() {
            "rw"
        } else {
            "r"
        };
        meta.insert("type".into(), PackValue::str(kind));
        meta.insert("t_last".into(), PackValue::Int(p.started.elapsed().as_millis() as i64));
        meta.insert("db".into(), PackValue::String(p.db.clone()));
        if p.stats.contains_updates() {
            meta.insert("stats".into(), stats(&p.stats));
        }
        if let Some(plan) = &p.plan {
            let key = if plan.profiled { "profile" } else { "plan" };
            meta.insert(key.into(), plan_tree(plan));
        }
        if self.at_least(5, 6) {
            // The GQL status of the result (5.6+; there are no notifications).
            let (status, description) = match (p.has_columns, p.has_rows) {
                (false, _) => ("00001", "note: successful completion - omitted result"),
                (true, false) => ("02000", "note: no data"),
                (true, true) => ("00000", "note: successful completion"),
            };
            meta.insert(
                "statuses".into(),
                PackValue::List(vec![PackValue::map([
                    ("gql_status", PackValue::str(status)),
                    ("status_description", PackValue::str(description)),
                    ("diagnostic_record", diagnostic_record(&[])),
                ])]),
            );
        }
        meta
    }

    // ---- explicit transactions ----

    fn begin(&mut self, fields: Fields) -> io::Result<Flow> {
        let extra = match fields.into_iter().next() {
            None | Some(PackValue::Null) => BTreeMap::new(),
            Some(PackValue::Map(m)) => m,
            Some(other) => {
                self.fail(Failure::invalid(format!("BEGIN takes a map, got {}", other.type_name())))?;
                return Ok(Flow::Continue);
            }
        };
        let home = no_db(&extra) && self.at_least(5, 8);
        let prepared = self.database_name(&extra).and_then(|name| {
            let (who, _) = self.access()?;
            let ctx = &self.shared.ctx;
            let access = if self.is_system(&name) { ctx.best_access(who) } else { ctx.access_on(who, &name) };
            api::check_as(who, access, Access::Read, "a transaction").map_err(|e| Failure::from_error(&e))?;
            let limit = self.time_limit(&extra)?;
            if self.is_system(&name) {
                return Ok((name, limit, None));
            }
            let db = self
                .shared
                .ctx
                .mgr
                .database(&name)
                .map_err(|e| Failure::new(DATABASE_NOT_FOUND, e.to_string()))?;
            Ok((name, limit, Some(db)))
        });
        let (name, limit, db) = match prepared {
            Ok(v) => v,
            Err(f) => {
                self.fail(f)?;
                return Ok(Flow::Continue);
            }
        };
        let deadline = limit.map(|l| Instant::now() + l);
        let Some(db) = db else {
            // A `system` transaction: each database command takes effect when it runs.
            self.success(BTreeMap::new())?;
            return self.transaction(None, &name, deadline);
        };
        match self.interruptible(limit, |options| db.begin_with_options(options)) {
            None => {
                self.ignored()?;
                Ok(Flow::Continue)
            }
            Some(Err(e)) => {
                self.fail(Failure::from_error(&e))?;
                Ok(Flow::Continue)
            }
            Some(Ok(tx)) => {
                let mut meta = BTreeMap::new();
                if home {
                    // The home database it runs on, for the driver's cache (5.8+).
                    meta.insert("db".into(), PackValue::String(name.clone()));
                }
                self.success(meta)?;
                self.transaction(Some(tx), &name, deadline)
            }
        }
    }

    /// Serves messages inside a transaction until it ends. `None` is a transaction on
    /// the `system` database, which runs database commands at once, outside any store
    /// transaction: committing it does nothing more and rolling it back undoes nothing.
    fn transaction(
        &mut self,
        tx: Option<Transaction<'_, BoxedStore>>,
        name: &str,
        deadline: Option<Instant>,
    ) -> io::Result<Flow> {
        // A store transaction is listed for SHOW TRANSACTIONS, and can be terminated.
        let listed = tx.as_ref().map(|_| {
            let stop = self.shared.ctx.abort.child();
            self.tx_stop = Some(stop.clone());
            let info = Info {
                database: name.to_string(),
                who: self.who.clone().unwrap_or(Principal::Open),
                protocol: "bolt",
                connection: self.connection.clone(),
                client: self.peer.clone(),
            };
            self.shared.ctx.running.begin(info, Stop::Cancel(stop), None)
        });
        let flow = self.serve_transaction(tx, name, deadline, listed.as_ref());
        self.tx_stop = None;
        drop(listed);
        flow
    }

    fn serve_transaction(
        &mut self,
        mut tx: Option<Transaction<'_, BoxedStore>>,
        name: &str,
        deadline: Option<Instant>,
        listed: Option<&Guard>,
    ) -> io::Result<Flow> {
        let mut results: Vec<Option<Pending>> = Vec::new();
        let idle = self.shared.ctx.txs.timeout();
        loop {
            let (tag, fields) = match self.next(Some(Instant::now() + idle))? {
                Event::Message(t, f) => (t, f),
                Event::Close => return Ok(Flow::Close),
                Event::Terminated => {
                    if let Some(tx) = tx {
                        tx.rollback();
                    }
                    self.pending_failure = Some(Failure::new(
                        "Neo.ClientError.Transaction.Terminated",
                        "the transaction was terminated (TERMINATE TRANSACTIONS) and rolled back",
                    ));
                    return Ok(Flow::Continue);
                }
                Event::Idle => {
                    if let Some(tx) = tx {
                        tx.rollback();
                    }
                    self.pending_failure = Some(Failure::new(
                        TIMED_OUT,
                        format!(
                            "the transaction was rolled back after {} ms without a message",
                            idle.as_millis()
                        ),
                    ));
                    return Ok(Flow::Continue);
                }
            };
            match tag {
                GOODBYE => return Ok(Flow::Close),
                RESET => {
                    if let Some(tx) = tx {
                        tx.rollback();
                    }
                    self.reset()?;
                    return Ok(Flow::Continue);
                }
                _ if self.control.reset_pending() => self.ignored()?,
                RUN => {
                    let started = Instant::now();
                    let prepared = Self::run_fields(fields).and_then(|(text, params, _)| {
                        let source = text.clone();
                        let (who, access) = self.access()?;
                        let ctx = &self.shared.ctx;
                        let mut action = None;
                        let statement = match tx {
                            Some(_) => api::transaction_query(name, &text, &params).and_then(|q| {
                                let (needed, what) = api::authorize_query(&ctx.mgr, name, &q);
                                api::check_as(who, ctx.access_on(who, name), needed, what)?;
                                Ok(TxStatement::Query(q))
                            }),
                            None => graphbuff::cypher::parse_statement(&text).and_then(|s| {
                                action = api::audit_action(&s, &params);
                                ctx.authorize(who, &s, name, &params)?;
                                system_command(&s)?;
                                Ok(match s {
                                    Statement::User(cmd) => TxStatement::User(cmd),
                                    _ => TxStatement::System(text),
                                })
                            }),
                        };
                        if let (Some(a), Err(e)) = (&action, &statement) {
                            ctx.audit("bolt", &self.peer, who, a, Some(e as &dyn std::fmt::Display));
                        }
                        Ok((statement.map_err(|e| Failure::from_error(&e))?, params, access, source, action))
                    });
                    let (query, params, access, source, action) = match prepared {
                        Ok(v) => v,
                        Err(f) => {
                            self.fail(f)?;
                            return Ok(Flow::Continue);
                        }
                    };
                    let mut timeout = self.shared.ctx.query_timeout;
                    if let Some(d) = deadline {
                        let left = d.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
                        timeout = Some(timeout.map_or(left, |t| t.min(left)));
                    }
                    let ctx = &self.shared.ctx;
                    let who = self.who.clone().unwrap_or(Principal::Open);
                    let run = |options: &QueryOptions| match (&mut tx, &query) {
                        (Some(tx), TxStatement::Query(q)) => tx.execute_query_with_options(q, &params, options),
                        (_, TxStatement::System(text)) => ctx.mgr.session().execute_with_options(text, &params, options),
                        (_, TxStatement::User(cmd)) => ctx.user_command(&who, access, cmd, &params),
                        (None, TxStatement::Query(_)) => unreachable!("system transactions only take database commands"),
                    };
                    if let Some(l) = listed {
                        l.query(Some(&source));
                    }
                    let result = self.interruptible(timeout, run);
                    if let Some(l) = listed {
                        l.query(None);
                    }
                    if let Some(r) = &result {
                        let who = self.who.clone().unwrap_or(Principal::Open);
                        ctx.finished("bolt", name, &who, &source, started.elapsed(), r.as_ref().err());
                        if let Some(a) = &action {
                            let failure = r.as_ref().err().map(|e| e as &dyn std::fmt::Display);
                            ctx.audit("bolt", &self.peer, &who, a, failure);
                        }
                    }
                    match result {
                        None => self.ignored()?,
                        Some(Err(e)) => {
                            self.fail(Failure::from_error(&e))?;
                            return Ok(Flow::Continue);
                        }
                        Some(Ok(r)) => {
                            let qid = results.len() as i64;
                            let (pending, columns) = Pending::new(r, name.to_string(), started);
                            results.push(Some(pending));
                            self.success(run_success(columns, started, Some(qid)))?;
                        }
                    }
                }
                PULL | DISCARD => {
                    let qid = match fields.first() {
                        Some(PackValue::Map(m)) => match m.get("qid") {
                            None | Some(PackValue::Int(-1)) => results.len() as i64 - 1,
                            Some(PackValue::Int(q)) => *q,
                            Some(_) => -2,
                        },
                        _ => -2,
                    };
                    let slot = usize::try_from(qid).ok().and_then(|q| results.get_mut(q));
                    let Some(Some(mut p)) = slot.map(Option::take) else {
                        self.fail(Failure::invalid(format!("there is no open result with qid {qid}")))?;
                        return Ok(Flow::Continue);
                    };
                    if !self.stream(&mut p, tag == PULL, &fields, false)? {
                        if self.failed {
                            return Ok(Flow::Continue);
                        }
                        results[qid as usize] = Some(p);
                    }
                }
                COMMIT => {
                    match tx.map_or(Ok(()), |tx| tx.commit()) {
                        Ok(()) => {
                            let bookmark = PackValue::String(new_bookmark());
                            self.success(BTreeMap::from([("bookmark".into(), bookmark)]))?;
                        }
                        Err(e) => self.fail(Failure::from_error(&e))?,
                    }
                    return Ok(Flow::Continue);
                }
                ROLLBACK => {
                    if let Some(tx) = tx {
                        tx.rollback();
                    }
                    self.success(BTreeMap::new())?;
                    return Ok(Flow::Continue);
                }
                TELEMETRY if self.at_least(5, 4) => self.success(BTreeMap::new())?,
                BEGIN => {
                    self.fail(Failure::invalid("a transaction is already open"))?;
                    return Ok(Flow::Continue);
                }
                other => {
                    self.fail(Failure::invalid(format!(
                        "message 0x{other:02X} is not allowed in a transaction"
                    )))?;
                    return Ok(Flow::Continue);
                }
            }
        }
    }

    // ---- routing ----

    /// `ROUTE`: this server is the only member, and serves every role.
    fn route(&mut self, fields: Fields) -> io::Result<()> {
        let mut it = fields.into_iter();
        let routing = it.next();
        let _bookmarks = it.next();
        let db = match it.next() {
            Some(PackValue::Map(extra)) => extra,
            Some(PackValue::String(s)) => BTreeMap::from([("db".to_string(), PackValue::String(s))]),
            _ => BTreeMap::new(),
        };
        if let Some(PackValue::String(_)) = db.get("imp_user") {
            return self.fail(Failure::new(NOT_SUPPORTED, "impersonation (imp_user) is not supported"));
        }
        let name = match self.database_name(&db) {
            Ok(n) => n,
            Err(f) => return self.fail(f),
        };
        // The address the client used, if it said; else the one it connected to.
        let address = match &routing {
            Some(PackValue::Map(m)) => match m.get("address") {
                Some(PackValue::String(a)) if !a.is_empty() => a.clone(),
                _ => self.local.to_string(),
            },
            _ => self.local.to_string(),
        };
        let servers = ["WRITE", "READ", "ROUTE"]
            .into_iter()
            .map(|role| {
                PackValue::map([
                    ("addresses", PackValue::List(vec![PackValue::String(address.clone())])),
                    ("role", PackValue::str(role)),
                ])
            })
            .collect();
        let table = PackValue::map([
            ("ttl", PackValue::Int(300)),
            ("db", PackValue::String(name)),
            ("servers", PackValue::List(servers)),
        ]);
        self.success(BTreeMap::from([("rt".into(), table)]))
    }
}

/// A statement in a Bolt transaction.
/// The rows `PULL` or `DISCARD` ask for: `n`, or -1 for all.
fn pull_size(fields: &[PackValue]) -> Result<usize, Failure> {
    match fields.first() {
        Some(PackValue::Map(m)) => match m.get("n") {
            Some(PackValue::Int(-1)) => Ok(usize::MAX),
            Some(PackValue::Int(n)) if *n > 0 => Ok(*n as usize),
            _ => Err(Failure::invalid("`n` must be a positive number of rows, or -1 for all")),
        },
        _ => Err(Failure::invalid("PULL and DISCARD take a map")),
    }
}

/// A `PULL` or `DISCARD` being answered.
struct Request {
    /// Rows it still takes.
    left: usize,
    /// `PULL` (send them) or `DISCARD`.
    send: bool,
}

/// Why a stream ended before its query did.
enum Ended {
    /// `DISCARD` of all the rows that are left: the query stops, the summary answers it.
    Discarded,
    /// The request was answered with a failure, or the next message will be (the query
    /// was terminated or timed out while waiting for it): the query stops.
    Answered,
    /// Another message came (kept in [`Conn::stashed`]): the query stops.
    Stashed,
    /// The client left or the server is stopping.
    Close,
    Io(io::Error),
}

/// Streams an auto-commit query's rows to the client: `RUN`'s `SUCCESS` (with the columns)
/// goes out with the first row, so that a query failing before it fails the `RUN` as
/// before; then every row waits for a `PULL` (or `DISCARD`) with rows left to take, so
/// rows go out only as the client asks for them.
struct Streamer<'c, 's> {
    conn: &'c mut Conn<'s>,
    started: Instant,
    /// The home database the query ran on (5.8+), sent with the `RUN`'s `SUCCESS`.
    resolved: Option<String>,
    /// When the query's time is up.
    deadline: Option<Instant>,
    columns: Vec<String>,
    /// The `RUN` was answered.
    opened: bool,
    /// The request being answered, once the `RUN` is.
    request: Option<Request>,
    ended: Option<Ended>,
}

impl Streamer<'_, '_> {
    /// Stops the query: the reason is in `ended`.
    fn stop(&mut self, why: Ended) -> graphbuff::Result<()> {
        self.ended = Some(why);
        Err(Error::Cancelled("the result was closed".into()))
    }

    fn io(&mut self, r: io::Result<()>) -> graphbuff::Result<()> {
        match r {
            Ok(()) => Ok(()),
            Err(e) => self.stop(Ended::Io(e)),
        }
    }

    /// Waits for the next `PULL` or `DISCARD`. `ROUTE` and `TELEMETRY` are answered on
    /// the way; anything else ends the stream. A query terminated or out of time while it
    /// waits stops (giving up its snapshot); the next message gets the failure.
    fn await_request(&mut self) -> graphbuff::Result<()> {
        loop {
            let poll = Instant::now() + POLL;
            let event = match self.conn.next(Some(self.deadline.map_or(poll, |d| d.min(poll)))) {
                Ok(e) => e,
                Err(e) => return self.stop(Ended::Io(e)),
            };
            let (tag, fields) = match event {
                Event::Message(tag, fields) => (tag, fields),
                Event::Idle => {
                    let cancelled = self.conn.control.running().as_ref().is_some_and(CancelHandle::is_cancelled);
                    let error = if cancelled {
                        Error::Cancelled("the query was cancelled".into())
                    } else if let Some(d) = self.deadline.filter(|d| Instant::now() >= *d) {
                        let limit = d.duration_since(self.started).as_millis();
                        Error::Timeout(format!("the query ran longer than its time limit of {limit} ms"))
                    } else {
                        continue;
                    };
                    self.conn.pending_failure = Some(Failure::from_error(&error));
                    return self.stop(Ended::Answered);
                }
                Event::Close | Event::Terminated => return self.stop(Ended::Close),
            };
            match tag {
                PULL | DISCARD if !self.conn.control.reset_pending() => {
                    let left = match pull_size(&fields) {
                        Ok(n) => n,
                        Err(f) => {
                            let r = self.conn.fail(f);
                            self.io(r)?;
                            return self.stop(Ended::Answered);
                        }
                    };
                    self.request = Some(Request { left, send: tag == PULL });
                    if tag == DISCARD && left == usize::MAX {
                        return self.stop(Ended::Discarded);
                    }
                    return Ok(());
                }
                ROUTE if !self.conn.control.reset_pending() => {
                    let r = self.conn.route(fields);
                    self.io(r)?;
                }
                TELEMETRY if self.conn.at_least(5, 4) && !self.conn.control.reset_pending() => {
                    let r = self.conn.success(BTreeMap::new());
                    self.io(r)?;
                }
                _ => {
                    self.conn.stashed = Some((tag, fields));
                    return self.stop(Ended::Stashed);
                }
            }
        }
    }
}

impl RowSink for Streamer<'_, '_> {
    fn columns(&mut self, columns: &[String]) -> graphbuff::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }

    fn row(&mut self, row: Vec<ResultValue>) -> graphbuff::Result<()> {
        if !self.opened {
            self.opened = true;
            let mut meta = run_success(std::mem::take(&mut self.columns), self.started, None);
            if let Some(db) = self.resolved.take() {
                meta.insert("db".into(), PackValue::String(db));
            }
            let r = self.conn.success(meta);
            self.io(r)?;
            self.await_request()?;
        } else if self.request.as_ref().is_some_and(|r| r.left == 0) {
            let more = BTreeMap::from([("has_more".into(), PackValue::Bool(true))]);
            let r = self.conn.success(more);
            self.io(r)?;
            self.await_request()?;
        }
        let request = self.request.as_mut().expect("opening waited for a request");
        request.left -= 1;
        if request.send {
            let enc = Encoder {
                element_ids: self.conn.version.0 >= 5,
                utc: self.conn.utc,
            };
            let values = row.iter().map(|v| enc.result(v)).collect();
            let r = self.conn.send(RECORD, vec![PackValue::List(values)]);
            self.io(r)?;
        }
        Ok(())
    }
}

enum TxStatement {
    Query(Query),
    /// A database command, on the `system` database.
    System(String),
    /// A user command, on the `system` database.
    User(UserCommand),
}

/// Checks that `stmt` is a database or user command, the only statements the `system`
/// database runs.
fn system_command(stmt: &Statement) -> Result<(), Error> {
    match stmt {
        Statement::Database(_) | Statement::User(_) => Ok(()),
        _ => Err(Error::Unsupported(
            "the system database only runs database and user commands (CREATE DATABASE, CREATE USER, …)".into(),
        )),
    }
}

/// The element id of a node, as `elementId()` returns it.
fn node_element_id(id: u64) -> String {
    format!("n:{id}")
}

/// The element id of a relationship, as `elementId()` returns it.
fn rel_element_id(id: u64) -> String {
    format!("r:{id}")
}

/// Whether an `extra` map leaves the database to the server (no `db`, null or empty).
fn no_db(extra: &BTreeMap<String, PackValue>) -> bool {
    match extra.get("db") {
        None | Some(PackValue::Null) => true,
        Some(PackValue::String(s)) => s.is_empty(),
        Some(_) => false,
    }
}

fn new_bookmark() -> String {
    format!("graphbuff:{}", BOOKMARKS.fetch_add(1, Ordering::Relaxed) + 1)
}

fn run_success(columns: Vec<String>, started: Instant, qid: Option<i64>) -> BTreeMap<String, PackValue> {
    let mut meta = BTreeMap::new();
    meta.insert(
        "fields".into(),
        PackValue::List(columns.into_iter().map(PackValue::String).collect()),
    );
    meta.insert("t_first".into(), PackValue::Int(started.elapsed().as_millis() as i64));
    if let Some(q) = qid {
        meta.insert("qid".into(), PackValue::Int(q));
    }
    meta
}

/// The counters that are not zero, under Neo4j's names.
fn stats(s: &QueryStats) -> PackValue {
    let counters = [
        ("nodes-created", s.nodes_created),
        ("nodes-deleted", s.nodes_deleted),
        ("relationships-created", s.relationships_created),
        ("relationships-deleted", s.relationships_deleted),
        ("properties-set", s.properties_set),
        ("labels-added", s.labels_added),
        ("labels-removed", s.labels_removed),
        ("indexes-added", s.indexes_added),
        ("indexes-removed", s.indexes_removed),
        ("constraints-added", s.constraints_added),
        ("constraints-removed", s.constraints_removed),
        ("system-updates", s.system_updates),
    ];
    let mut m: BTreeMap<String, PackValue> = counters
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| (k.to_string(), PackValue::Int(n as i64)))
        .collect();
    m.insert("contains-updates".into(), PackValue::Bool(s.contains_updates()));
    PackValue::Map(m)
}

/// A plan as Neo4j's nested map: `operatorType`, `arguments`, `identifiers`, `children`,
/// and for `PROFILE` the `rows` and `time` (nanoseconds) of each operator.
fn plan_tree(plan: &PlanDescription) -> PackValue {
    fn node(plan: &PlanDescription, i: &mut usize) -> PackValue {
        let line = &plan.lines[*i];
        *i += 1;
        let (op, details) = line.operator.split_once(' ').unwrap_or((&line.operator, ""));
        let mut args = BTreeMap::new();
        if !details.is_empty() {
            args.insert("Details".to_string(), PackValue::str(details));
        }
        if let Some(e) = line.estimated_rows {
            args.insert("EstimatedRows".to_string(), PackValue::Float(e));
        }
        let mut m = BTreeMap::new();
        m.insert("operatorType".to_string(), PackValue::str(op));
        m.insert("identifiers".to_string(), PackValue::List(vec![]));
        if plan.profiled {
            let rows = line.rows.unwrap_or(0) as i64;
            let time = line.time.map_or(0, |t| t.as_nanos() as i64);
            m.insert("rows".to_string(), PackValue::Int(rows));
            m.insert("dbHits".to_string(), PackValue::Int(0));
            m.insert("time".to_string(), PackValue::Int(time));
            args.insert("Rows".to_string(), PackValue::Int(rows));
            args.insert("Time".to_string(), PackValue::Int(time));
        }
        m.insert("arguments".to_string(), PackValue::Map(args));
        let mut children = Vec::new();
        while *i < plan.lines.len() && plan.lines[*i].depth == line.depth + 1 {
            children.push(node(plan, i));
        }
        m.insert("children".to_string(), PackValue::List(children));
        PackValue::Map(m)
    }
    if plan.lines.is_empty() {
        return PackValue::Map(BTreeMap::new());
    }
    node(plan, &mut 0)
}

// ---- values -------------------------------------------------------------------------------

/// Encodes result values for one protocol version.
struct Encoder {
    /// Bolt 5: nodes and relationships carry string element ids too.
    element_ids: bool,
    /// Date-times as UTC instants (`I`), not local seconds (`F`).
    utc: bool,
}

fn structure(tag: u8, fields: Vec<PackValue>) -> PackValue {
    PackValue::Struct { tag, fields }
}

impl Encoder {
    fn result(&self, v: &ResultValue) -> PackValue {
        match v {
            ResultValue::Null => PackValue::Null,
            ResultValue::Bool(b) => PackValue::Bool(*b),
            ResultValue::Int(i) => PackValue::Int(*i),
            ResultValue::Float(f) => PackValue::Float(*f),
            ResultValue::String(s) => PackValue::String(s.clone()),
            ResultValue::List(items) => PackValue::List(items.iter().map(|x| self.result(x)).collect()),
            ResultValue::Map(m) => PackValue::Map(m.iter().map(|(k, x)| (k.clone(), self.result(x))).collect()),
            ResultValue::Node(n) => self.node(n),
            ResultValue::Relationship(r) => self.relationship(r),
            ResultValue::Path(p) => self.path(p),
            ResultValue::Temporal(t) => self.temporal(t),
            ResultValue::Point(p) => point(p),
        }
    }

    fn value(&self, v: &Value) -> PackValue {
        match v {
            Value::Null => PackValue::Null,
            Value::Bool(b) => PackValue::Bool(*b),
            Value::Int(i) => PackValue::Int(*i),
            Value::Float(f) => PackValue::Float(*f),
            Value::String(s) => PackValue::String(s.clone()),
            Value::List(items) => PackValue::List(items.iter().map(|x| self.value(x)).collect()),
            Value::Map(m) => self.props(m),
            Value::Temporal(t) => self.temporal(t),
            Value::Point(p) => point(p),
            // Not storable as properties, so never part of a materialized result's.
            Value::Node(_) | Value::Rel(_) | Value::Path(_) => PackValue::Null,
        }
    }

    fn props(&self, m: &BTreeMap<String, Value>) -> PackValue {
        PackValue::Map(m.iter().map(|(k, x)| (k.clone(), self.value(x))).collect())
    }

    fn node(&self, n: &NodeData) -> PackValue {
        let mut fields = vec![
            PackValue::Int(n.id.0 as i64),
            PackValue::List(n.labels.iter().cloned().map(PackValue::String).collect()),
            self.props(&n.properties),
        ];
        if self.element_ids {
            fields.push(PackValue::String(node_element_id(n.id.0)));
        }
        structure(NODE, fields)
    }

    fn relationship(&self, r: &RelData) -> PackValue {
        let mut fields = vec![
            PackValue::Int(r.id.0 as i64),
            PackValue::Int(r.start.0 as i64),
            PackValue::Int(r.end.0 as i64),
            PackValue::String(r.rel_type.clone()),
            self.props(&r.properties),
        ];
        if self.element_ids {
            fields.push(PackValue::String(rel_element_id(r.id.0)));
            fields.push(PackValue::String(node_element_id(r.start.0)));
            fields.push(PackValue::String(node_element_id(r.end.0)));
        }
        structure(RELATIONSHIP, fields)
    }

    /// A path as its distinct nodes, distinct relationships (without endpoints) and a
    /// sequence of indices: for each step, the relationship (1-based, negative when
    /// walked against its direction) and the node it leads to.
    fn path(&self, p: &PathData) -> PackValue {
        let mut nodes: Vec<&NodeData> = Vec::new();
        let mut rels: Vec<&RelData> = Vec::new();
        let mut indices = Vec::new();
        if let Some(first) = p.nodes.first() {
            nodes.push(first);
        }
        for (i, r) in p.relationships.iter().enumerate() {
            let (Some(from), Some(to)) = (p.nodes.get(i), p.nodes.get(i + 1)) else {
                break;
            };
            let ri = match rels.iter().position(|x| x.id == r.id) {
                Some(k) => k as i64 + 1,
                None => {
                    rels.push(r);
                    rels.len() as i64
                }
            };
            let forward = r.start == from.id && r.end == to.id;
            indices.push(PackValue::Int(if forward { ri } else { -ri }));
            let ni = match nodes.iter().position(|m| m.id == to.id) {
                Some(k) => k as i64,
                None => {
                    nodes.push(to);
                    nodes.len() as i64 - 1
                }
            };
            indices.push(PackValue::Int(ni));
        }
        let rels = rels
            .into_iter()
            .map(|r| {
                let mut fields = vec![
                    PackValue::Int(r.id.0 as i64),
                    PackValue::String(r.rel_type.clone()),
                    self.props(&r.properties),
                ];
                if self.element_ids {
                    fields.push(PackValue::String(rel_element_id(r.id.0)));
                }
                structure(UNBOUND_RELATIONSHIP, fields)
            })
            .collect();
        structure(
            PATH,
            vec![
                PackValue::List(nodes.into_iter().map(|n| self.node(n)).collect()),
                PackValue::List(rels),
                PackValue::List(indices),
            ],
        )
    }

    fn temporal(&self, t: &Temporal) -> PackValue {
        let split = |ns: i128| {
            (
                PackValue::Int(ns.div_euclid(NANOS_PER_SECOND as i128) as i64),
                PackValue::Int(ns.rem_euclid(NANOS_PER_SECOND as i128) as i64),
            )
        };
        let local = |days: i64, nanos: i64| days as i128 * NANOS_PER_DAY as i128 + nanos as i128;
        match *t {
            Temporal::Date(d) => structure(DATE, vec![PackValue::Int(d)]),
            Temporal::LocalTime(n) => structure(LOCAL_TIME, vec![PackValue::Int(n)]),
            Temporal::Time { nanos, offset } => {
                structure(TIME, vec![PackValue::Int(nanos), PackValue::Int(offset as i64)])
            }
            Temporal::LocalDateTime { days, nanos } => {
                let (s, n) = split(local(days, nanos));
                structure(LOCAL_DATE_TIME, vec![s, n])
            }
            Temporal::DateTime { days, nanos, offset, zone: Some(z) } => {
                let mut ns = local(days, nanos);
                let tag = if self.utc {
                    ns -= offset as i128 * NANOS_PER_SECOND as i128;
                    DATE_TIME_ZONE_ID
                } else {
                    DATE_TIME_ZONE_ID_LEGACY
                };
                let (s, n) = split(ns);
                structure(tag, vec![s, n, PackValue::str(z.name())])
            }
            Temporal::DateTime { days, nanos, offset, zone: None } => {
                let mut ns = local(days, nanos);
                let tag = if self.utc {
                    ns -= offset as i128 * NANOS_PER_SECOND as i128;
                    DATE_TIME
                } else {
                    DATE_TIME_LEGACY
                };
                let (s, n) = split(ns);
                structure(tag, vec![s, n, PackValue::Int(offset as i64)])
            }
            Temporal::Duration(d) => structure(
                DURATION,
                vec![
                    PackValue::Int(d.months),
                    PackValue::Int(d.days),
                    PackValue::Int(d.seconds),
                    PackValue::Int(d.nanos as i64),
                ],
            ),
        }
    }
}

fn point(p: &Point) -> PackValue {
    let mut fields = vec![PackValue::Int(p.crs().srid())];
    fields.extend(p.coords().iter().map(|c| PackValue::Float(*c)));
    structure(if fields.len() == 4 { POINT_3D } else { POINT_2D }, fields)
}

/// A parameter value sent by a client.
fn to_value(v: PackValue) -> Result<Value, Failure> {
    Ok(match v {
        PackValue::Null => Value::Null,
        PackValue::Bool(b) => Value::Bool(b),
        PackValue::Int(i) => Value::Int(i),
        PackValue::Float(f) => Value::Float(f),
        PackValue::String(s) => Value::String(s),
        PackValue::Bytes(_) => {
            return Err(Failure::new(NOT_SUPPORTED, "byte array parameters are not supported"));
        }
        PackValue::List(items) => Value::List(items.into_iter().map(to_value).collect::<Result<_, _>>()?),
        PackValue::Map(m) => Value::Map(
            m.into_iter()
                .map(|(k, x)| Ok((k, to_value(x)?)))
                .collect::<Result<_, Failure>>()?,
        ),
        PackValue::Struct { tag, fields } => structure_value(tag, &fields)?,
    })
}

fn structure_value(tag: u8, fields: &[PackValue]) -> Result<Value, Failure> {
    let bad = |what: &str| Failure::new(TYPE_ERROR, format!("invalid {what} parameter"));
    let ints = |n: usize, what: &str| -> Result<Vec<i64>, Failure> {
        if fields.len() != n {
            return Err(bad(what));
        }
        fields
            .iter()
            .map(|f| match f {
                PackValue::Int(i) => Ok(*i),
                _ => Err(bad(what)),
            })
            .collect()
    };
    let check_offset = |o: i64, what: &str| -> Result<i32, Failure> {
        if o.abs() <= 18 * 3600 { Ok(o as i32) } else { Err(bad(what)) }
    };
    // Days and time of day from nanoseconds since the epoch, with the year in range.
    let local = |ns: i128, what: &str| -> Result<(i64, i64), Failure> {
        let days = i64::try_from(ns.div_euclid(NANOS_PER_DAY as i128)).map_err(|_| bad(what))?;
        check_days(days).ok_or_else(|| bad(what))?;
        Ok((days, ns.rem_euclid(NANOS_PER_DAY as i128) as i64))
    };
    let epoch = |s: i64, n: i64| s as i128 * NANOS_PER_SECOND as i128 + n as i128;
    let t = match tag {
        DATE => {
            let v = ints(1, "date")?;
            Temporal::Date(check_days(v[0]).ok_or_else(|| bad("date"))?)
        }
        LOCAL_TIME => {
            let v = ints(1, "local time")?;
            Temporal::LocalTime(time_of_day(v[0]).ok_or_else(|| bad("local time"))?)
        }
        TIME => {
            let v = ints(2, "time")?;
            Temporal::Time {
                nanos: time_of_day(v[0]).ok_or_else(|| bad("time"))?,
                offset: check_offset(v[1], "time")?,
            }
        }
        LOCAL_DATE_TIME => {
            let v = ints(2, "local date-time")?;
            let (days, nanos) = local(epoch(v[0], v[1]), "local date-time")?;
            Temporal::LocalDateTime { days, nanos }
        }
        DATE_TIME | DATE_TIME_LEGACY => {
            let v = ints(3, "date-time")?;
            let offset = check_offset(v[2], "date-time")?;
            let mut ns = epoch(v[0], v[1]);
            if tag == DATE_TIME {
                ns += offset as i128 * NANOS_PER_SECOND as i128; // UTC to local
            }
            let (days, nanos) = local(ns, "date-time")?;
            Temporal::DateTime { days, nanos, offset, zone: None }
        }
        DURATION => {
            let v = ints(4, "duration")?;
            let d = temporal::Duration::from_nanos(v[0], v[1], epoch(v[2], v[3])).map_err(|_| bad("duration"))?;
            Temporal::Duration(d)
        }
        DATE_TIME_ZONE_ID | DATE_TIME_ZONE_ID_LEGACY => {
            let (Some(PackValue::Int(s)), Some(PackValue::Int(n)), Some(PackValue::String(name)), 3) =
                (fields.first(), fields.get(1), fields.get(2), fields.len())
            else {
                return Err(bad("date-time"));
            };
            let zone = graphbuff::Tz::find(name)
                .ok_or_else(|| Failure::new(TYPE_ERROR, format!("unknown time zone `{name}`")))?;
            let ns = epoch(*s, *n);
            if tag == DATE_TIME_ZONE_ID {
                Temporal::at_instant_in(ns, zone).map_err(|_| bad("date-time"))?
            } else {
                let (days, nanos) = local(ns, "date-time")?;
                Temporal::in_zone(days, nanos, zone, None).map_err(|_| bad("date-time"))?
            }
        }
        POINT_2D | POINT_3D => {
            let dims = if tag == POINT_2D { 2 } else { 3 };
            let (Some(PackValue::Int(srid)), coords) = (fields.first(), &fields[1.min(fields.len())..]) else {
                return Err(bad("point"));
            };
            let coords: Vec<f64> = coords
                .iter()
                .map(|c| match c {
                    PackValue::Float(f) => Ok(*f),
                    PackValue::Int(i) => Ok(*i as f64),
                    _ => Err(bad("point")),
                })
                .collect::<Result<_, _>>()?;
            let crs = Crs::from_srid(*srid).filter(|c| c.dims() == dims).ok_or_else(|| bad("point"))?;
            if coords.len() != dims {
                return Err(bad("point"));
            }
            let p = Point::new(crs, &coords).map_err(|e| Failure::from_error(&e))?;
            return Ok(Value::Point(p));
        }
        NODE | RELATIONSHIP | UNBOUND_RELATIONSHIP | PATH => {
            return Err(Failure::new(TYPE_ERROR, "nodes, relationships and paths cannot be parameters"));
        }
        other => return Err(Failure::new(TYPE_ERROR, format!("unknown structure 0x{other:02X} in the parameters"))),
    };
    Ok(Value::Temporal(t))
}

fn time_of_day(nanos: i64) -> Option<i64> {
    (0..NANOS_PER_DAY).contains(&nanos).then_some(nanos)
}

/// `days` if its year is within the supported range.
fn check_days(days: i64) -> Option<i64> {
    let lowest = temporal::days_from_civil(-temporal::MAX_YEAR, 1, 1);
    let highest = temporal::days_from_civil(temporal::MAX_YEAR, 12, 31);
    (lowest..=highest).contains(&days).then_some(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_the_newest_common_version() {
        let p = |v: &[[u8; 4]]| v.concat();
        let v = |major, minor| Some(Handshake::Version(major, minor));
        assert_eq!(negotiate(&p(&[[0, 8, 8, 5], [0, 2, 4, 4], [0, 0, 0, 0], [0, 0, 0, 0]])), v(5, 8));
        assert_eq!(negotiate(&p(&[[0, 4, 9, 5], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]])), v(5, 8));
        assert_eq!(negotiate(&p(&[[0, 2, 4, 4], [0, 0, 2, 5], [0, 0, 0, 0], [0, 0, 0, 0]])), v(4, 4));
        // 5.5 is never served: 5.5–5.4 gives 5.4, 5.5 alone nothing.
        assert_eq!(negotiate(&p(&[[0, 1, 5, 5], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]])), v(5, 4));
        assert_eq!(negotiate(&p(&[[0, 0, 5, 5], [0, 0, 3, 4], [0, 0, 0, 0], [0, 0, 0, 0]])), None);
        assert_eq!(negotiate(&p(&[[0, 0, 6, 5], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]])), v(5, 6));
        assert_eq!(negotiate(&p(&[[0, 0, 4, 4], [0, 0, 3, 4], [0, 0, 0, 0], [0, 0, 0, 0]])), v(4, 4));
        assert_eq!(negotiate(&p(&[[0, 0, 3, 4], [0, 0, 0, 3], [0, 0, 0, 0], [0, 0, 0, 0]])), None);
        assert_eq!(negotiate(&p(&[[0, 0, 0, 0]; 4])), None);
        // The manifest, in the client's order of preference.
        assert_eq!(negotiate(&p(&[[0, 0, 1, 0xFF], [0, 0, 2, 5], [0, 0, 0, 0], [0, 0, 0, 0]])), Some(Handshake::Manifest));
        assert_eq!(negotiate(&p(&[[0, 0, 2, 5], [0, 0, 1, 0xFF], [0, 0, 0, 0], [0, 0, 0, 0]])), v(5, 2));
        assert_eq!(negotiate(&p(&[[0, 0, 2, 0xFF], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]])), None);
        assert_eq!(
            manifest(),
            [0, 0, 1, 0xFF, 3, 0, 2, 8, 5, 0, 4, 4, 5, 0, 0, 4, 4, 0]
        );
        assert!(served(5, 8) && served(5, 6) && served(5, 0) && served(4, 4));
        assert!(!served(5, 5) && !served(5, 9) && !served(4, 3) && !served(6, 0));
        assert_eq!(gql_status("Neo.ClientError.Statement.SyntaxError").0, "42001");
        assert_eq!(classification("Neo.TransientError.General.MemoryPoolOutOfMemoryError"), "TRANSIENT_ERROR");
    }

    #[test]
    fn temporals_round_trip() {
        let values = [
            Temporal::Date(-719_528),
            Temporal::LocalTime(3_600_000_000_123),
            Temporal::Time {
                nanos: 1,
                offset: -3600,
            },
            Temporal::LocalDateTime {
                days: -1,
                nanos: 5,
            },
            Temporal::DateTime {
                days: 20_000,
                nanos: 3_600 * NANOS_PER_SECOND + 7,
                offset: 7200,
                zone: None,
            },
            Temporal::in_zone(17_000, 3_600 * NANOS_PER_SECOND, graphbuff::Tz::find("Europe/Stockholm").unwrap(), None)
                .unwrap(),
            Temporal::Duration(temporal::Duration::from_nanos(14, -3, -1_500_000_000).unwrap()),
        ];
        for utc in [true, false] {
            let enc = Encoder {
                element_ids: true,
                utc,
            };
            for t in values {
                let PackValue::Struct { tag, fields } = enc.temporal(&t) else {
                    panic!()
                };
                let back = structure_value(tag, &fields).unwrap_or_else(|f| panic!("{}", f.message));
                assert_eq!(back, Value::Temporal(t), "utc={utc}");
            }
        }
    }

    #[test]
    fn invalid_temporals_are_refused() {
        assert!(structure_value(DATE, &[PackValue::Int(i64::MAX)]).is_err());
        assert!(structure_value(LOCAL_TIME, &[PackValue::Int(NANOS_PER_DAY)]).is_err());
        assert!(structure_value(TIME, &[PackValue::Int(0), PackValue::Int(19 * 3600)]).is_err());
        assert!(structure_value(DATE_TIME_ZONE_ID, &[]).is_err());
    }
}

/// Decodes a message and converts its fields to query values (see [`crate::fuzzing`]).
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_message(data: &[u8]) {
    let Ok(v) = packstream::decode(data) else { return };
    let fields = match v {
        PackValue::Struct { fields, .. } => fields,
        other => vec![other],
    };
    for f in fields {
        let _ = to_value(f);
    }
}
