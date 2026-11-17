//! Explicit transactions that span several HTTP requests.
//!
//! A [`graphbuff::Transaction`] holds its database's write lock, which belongs to the
//! thread that took it, while the requests of one transaction may arrive on any
//! connection. So each open transaction gets a thread of its own that owns it and runs
//! the statements sent to it over a channel. The thread rolls the transaction back when
//! no request arrives for the configured timeout, or when the server shuts down.
//!
//! A rollback request, and shutdown, cancel the statement the transaction is running (its
//! [`CancelHandle`]), so they do not wait for it to finish.

use std::collections::{BTreeMap, HashMap};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use graphbuff::cypher::ast::Query;
use graphbuff::{BoxedStore, CancelHandle, Database, Error, QueryOptions, QueryResult, Result, Value};

use crate::users::Principal;

/// What a transaction's thread knows about itself.
struct Started<'a> {
    id: &'a str,
    cancel: CancelHandle,
    committed: Arc<AtomicBool>,
}

/// What a request asks of a transaction's thread.
enum Command {
    Run {
        query: Box<Query>,
        params: BTreeMap<String, Value>,
        timeout: Option<Duration>,
        reply: Sender<Result<QueryResult>>,
    },
    Commit(Sender<Result<()>>),
    Rollback(Sender<()>),
}

/// Why a request could not reach its transaction.
pub(crate) enum TxError {
    /// No open transaction with that id on that database: it never existed, ended, or
    /// timed out.
    NotFound,
    /// The server is shutting down.
    Closed,
    /// A statement (or the commit) failed; the transaction was rolled back.
    Failed(Error),
}

struct Entry {
    db: String,
    /// Who began it: only they may use it.
    owner: Principal,
    commands: Sender<Command>,
    /// Cancels the statement the transaction runs.
    cancel: CancelHandle,
    /// Set once the transaction has committed.
    committed: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    open: HashMap<String, Entry>,
    /// Transaction threads still running (open, or rolling back).
    threads: usize,
    closed: bool,
}

/// The open transactions of a server.
pub(crate) struct Transactions {
    state: Mutex<State>,
    ended: Condvar,
    timeout: Duration,
    ids: AtomicU64,
    random: RandomState,
    /// Cancelled at shutdown: stops transactions waiting to begin.
    closing: CancelHandle,
}

impl Transactions {
    pub(crate) fn new(timeout: Duration) -> Arc<Transactions> {
        Arc::new(Transactions {
            state: Mutex::default(),
            ended: Condvar::new(),
            timeout: timeout.max(Duration::from_millis(1)),
            ids: AtomicU64::new(0),
            random: RandomState::new(),
            closing: CancelHandle::new(),
        })
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// An id that other clients cannot guess: 128 bits from the process's random hash
    /// keys, a counter and the clock.
    fn new_id(&self) -> String {
        let n = self.ids.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let half = |salt: u64| {
            let mut h = self.random.build_hasher();
            h.write_u64(salt);
            h.write_u64(n);
            h.write_u64(nanos);
            h.finish()
        };
        format!("{:016x}{:016x}", half(1), half(2))
    }

    /// Begins a transaction on `db` (called `name`), waiting (for at most `wait`, if
    /// set) while another transaction or writing query runs on it. Returns the new
    /// transaction's id.
    pub(crate) fn begin(
        self: &Arc<Self>,
        owner: &Principal,
        name: &str,
        db: Arc<Database<BoxedStore>>,
        wait: Option<Duration>,
    ) -> Result<String, TxError> {
        {
            let mut st = self.state();
            if st.closed {
                return Err(TxError::Closed);
            }
            st.threads += 1;
        }
        let (commands, inbox) = mpsc::channel();
        let (began, began_rx) = mpsc::channel();
        let id = self.new_id();
        let cancel = CancelHandle::new();
        let committed = Arc::new(AtomicBool::new(false));
        let me = Arc::clone(self);
        let thread_id = id.clone();
        let thread_cancel = cancel.clone();
        let thread_committed = Arc::clone(&committed);
        let begin_options = QueryOptions {
            timeout: wait,
            cancel: Some(self.closing.clone()),
        };
        let spawned = thread::Builder::new()
            .name("graphbuff-tx".into())
            .stack_size(crate::QUERY_STACK)
            .spawn(move || {
                let tx = Started {
                    id: &thread_id,
                    cancel: thread_cancel,
                    committed: thread_committed,
                };
                me.serve(tx, &db, &begin_options, inbox, began);
                let mut st = me.state();
                st.open.remove(&thread_id);
                st.threads -= 1;
                me.ended.notify_all();
            });
        if spawned.is_err() {
            let mut st = self.state();
            st.threads -= 1;
            self.ended.notify_all();
            return Err(TxError::Failed(Error::Runtime("cannot start a transaction thread".into())));
        }
        match began_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(Error::Cancelled(_))) if self.state().closed => return Err(TxError::Closed),
            Ok(Err(e)) => return Err(TxError::Failed(e)),
            Err(_) => return Err(TxError::Failed(Error::Runtime("the transaction thread stopped".into()))),
        }
        let mut st = self.state();
        if st.closed {
            // Dropping `commands` ends the thread, which rolls back.
            return Err(TxError::Closed);
        }
        st.open.insert(
            id.clone(),
            Entry {
                db: name.to_string(),
                owner: owner.clone(),
                commands,
                cancel,
                committed,
            },
        );
        Ok(id)
    }

    /// The thread of one transaction.
    fn serve(
        &self,
        started: Started<'_>,
        db: &Database<BoxedStore>,
        begin_options: &QueryOptions,
        inbox: Receiver<Command>,
        began: Sender<Result<()>>,
    ) {
        let id = started.id;
        let tx = match db.begin_with_options(begin_options) {
            Ok(tx) => tx,
            Err(e) => {
                let _ = began.send(Err(e));
                return;
            }
        };
        if began.send(Ok(())).is_err() {
            return;
        }
        let mut tx = Some(tx);
        let mut deadline = Instant::now() + self.timeout;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            let command = match inbox.recv_timeout(wait) {
                Ok(c) => c,
                // Abandoned, or the server is closing: dropping `tx` rolls back.
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return,
            };
            let open = tx.as_mut().expect("the loop ends with the transaction");
            match command {
                Command::Run {
                    query,
                    params,
                    timeout,
                    reply,
                } => {
                    let options = QueryOptions {
                        timeout,
                        cancel: Some(started.cancel.clone()),
                    };
                    let result = open.execute_query_with_options(&query, &params, &options);
                    let failed = result.is_err();
                    let _ = reply.send(result);
                    if failed {
                        return; // the transaction has rolled back
                    }
                }
                Command::Commit(reply) => {
                    // Unlist first, so that a request after the commit's reply finds nothing.
                    self.state().open.remove(id);
                    let result = tx.take().expect("open").commit();
                    if result.is_ok() {
                        started.committed.store(true, Ordering::Relaxed);
                    }
                    let _ = reply.send(result);
                    return;
                }
                Command::Rollback(reply) => {
                    self.state().open.remove(id);
                    tx.take().expect("open").rollback();
                    let _ = reply.send(());
                    return;
                }
            }
            deadline = Instant::now() + self.timeout;
        }
    }

    fn sender(&self, who: &Principal, db: &str, id: &str) -> Result<Sender<Command>, TxError> {
        let st = self.state();
        if st.closed {
            return Err(TxError::Closed);
        }
        match st.open.get(id) {
            Some(e) if e.db == db && e.owner == *who => Ok(e.commands.clone()),
            _ => Err(TxError::NotFound),
        }
    }

    /// Sends a command built around a reply channel and waits for the reply.
    fn ask<T>(
        &self,
        who: &Principal,
        db: &str,
        id: &str,
        command: impl FnOnce(Sender<T>) -> Command,
    ) -> Result<T, TxError> {
        let commands = self.sender(who, db, id)?;
        let (reply, answer) = mpsc::channel();
        commands.send(command(reply)).map_err(|_| TxError::NotFound)?;
        // No answer: the transaction ended (timed out, committed, rolled back) before it
        // got to this command.
        answer.recv().map_err(|_| TxError::NotFound)
    }

    pub(crate) fn run(
        &self,
        who: &Principal,
        db: &str,
        id: &str,
        query: Query,
        params: BTreeMap<String, Value>,
        timeout: Option<Duration>,
    ) -> Result<QueryResult, TxError> {
        let query = Box::new(query);
        self.ask(who, db, id, |reply| Command::Run {
            query,
            params,
            timeout,
            reply,
        })?
        .map_err(TxError::Failed)
    }

    pub(crate) fn commit(&self, who: &Principal, db: &str, id: &str) -> Result<(), TxError> {
        self.ask(who, db, id, Command::Commit)?.map_err(TxError::Failed)
    }

    /// Rolls back, cancelling the statement the transaction is running, if any.
    pub(crate) fn rollback(&self, who: &Principal, db: &str, id: &str) -> Result<(), TxError> {
        let (commands, committed) = {
            let mut st = self.state();
            if st.closed {
                return Err(TxError::Closed);
            }
            match st.open.get(id) {
                Some(e) if e.db == db && e.owner == *who => {}
                _ => return Err(TxError::NotFound),
            }
            // Unlisted now: later requests find nothing.
            let e = st.open.remove(id).expect("found");
            e.cancel.cancel();
            (e.commands, e.committed)
        };
        let (reply, answer) = mpsc::channel();
        let _ = commands.send(Command::Rollback(reply));
        match answer.recv() {
            Ok(()) => Ok(()),
            // The thread ended first: the cancelled statement failed (which rolled back),
            // the transaction timed out, or a commit sent earlier got there first.
            Err(_) if committed.load(Ordering::Relaxed) => Err(TxError::NotFound),
            Err(_) => Ok(()),
        }
    }

    /// Rolls back every open transaction and refuses new ones. Waits for the
    /// transaction threads to finish.
    pub(crate) fn close(&self) {
        let mut st = self.state();
        st.closed = true;
        self.closing.cancel();
        for e in st.open.values() {
            e.cancel.cancel();
        }
        st.open.clear(); // disconnects every transaction thread
        while st.threads > 0 {
            st = self.ended.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }
}
