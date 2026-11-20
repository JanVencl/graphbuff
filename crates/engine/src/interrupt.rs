//! Query timeouts, cancellation and memory limits.
//!
//! A query runs with [`QueryOptions`]: an optional time limit and an optional
//! [`CancelHandle`]. The executor checks them while it works (once per row produced or
//! candidate looked at, reading the clock every few hundred checks), so a long query
//! stops soon after its limit passes or it is cancelled, and fails with
//! [`Error::Timeout`] or [`Error::Cancelled`]. A memory limit is checked at the same
//! times, against the memory the query's thread took since the query started (see
//! [`graphbuff_core::accounting`]), and fails with [`Error::MemoryLimit`]. A writing query
//! that stops is rolled back like any failed query; in an explicit transaction the whole
//! transaction rolls back.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use graphbuff_core::clock::Instant;
use graphbuff_core::{Error, Result};

/// Cancels the queries it was given to, from any thread.
///
/// ```
/// use graphbuff::{CancelHandle, Database, QueryOptions};
///
/// let db = Database::in_memory();
/// let cancel = CancelHandle::new();
/// let options = QueryOptions::default().with_cancel(cancel.clone());
/// cancel.cancel(); // e.g. from another thread while the query runs
/// let err = db.execute_with_options("UNWIND range(1, 10) AS x RETURN x", &Default::default(), &options);
/// assert!(matches!(err, Err(graphbuff::Error::Cancelled(_))));
/// ```
#[derive(Clone, Debug, Default)]
pub struct CancelHandle(Arc<Flag>);

#[derive(Debug, Default)]
struct Flag {
    cancelled: AtomicBool,
    parent: Option<CancelHandle>,
}

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle that is cancelled on its own, or when this one is: cancelling the
    /// parent stops every query of its children, cancelling a child stops only its own.
    ///
    /// ```
    /// use graphbuff::CancelHandle;
    ///
    /// let all = CancelHandle::new();
    /// let (a, b) = (all.child(), all.child());
    /// a.cancel();
    /// assert!(a.is_cancelled() && !b.is_cancelled() && !all.is_cancelled());
    /// all.cancel();
    /// assert!(b.is_cancelled());
    /// ```
    pub fn child(&self) -> Self {
        CancelHandle(Arc::new(Flag {
            cancelled: AtomicBool::new(false),
            parent: Some(self.clone()),
        }))
    }

    /// Asks the queries running with this handle to stop, and makes later queries with
    /// it fail at once. Cannot be undone.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Relaxed) || self.0.parent.as_ref().is_some_and(CancelHandle::is_cancelled)
    }
}

/// Limits for one query (see [`crate::Database::execute_with_options`]).
#[derive(Clone, Debug, Default)]
pub struct QueryOptions {
    /// How long the query may take, counted from the call, including any wait for
    /// other writers. `None`: no limit.
    pub timeout: Option<Duration>,
    /// Stops the query when cancelled.
    pub cancel: Option<CancelHandle>,
    /// The most memory the query may hold, in bytes. Enforced only when the program's
    /// global allocator is [`crate::accounting::CountingAllocator`] (as in
    /// `graphbuff-server`). `None`: no limit.
    pub max_memory: Option<u64>,
}

impl QueryOptions {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_cancel(mut self, cancel: CancelHandle) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub fn with_max_memory(mut self, bytes: u64) -> Self {
        self.max_memory = Some(bytes);
        self
    }

    /// Whether these options limit the query at all.
    pub(crate) fn is_limited(&self) -> bool {
        self.timeout.is_some()
            || self.cancel.is_some()
            || (self.max_memory.is_some() && graphbuff_core::accounting::is_counting())
    }
}

/// How many checks pass between two reads of the clock.
const CLOCK_EVERY: u32 = 256;

/// How many checks pass between two looks at the memory the query holds.
const MEMORY_EVERY: u32 = 32;

/// The limits of a running query, checked by the executor.
#[derive(Clone, Debug)]
pub(crate) struct Interrupt {
    deadline: Option<(Instant, Duration)>,
    cancel: Option<CancelHandle>,
    memory: Option<MemoryLimit>,
    ticks: Cell<u32>,
}

/// A memory limit: what the query's thread held when it started, and how much more it
/// may hold.
#[derive(Clone, Debug)]
struct MemoryLimit {
    thread: std::thread::ThreadId,
    start: i64,
    limit: u64,
}

impl MemoryLimit {
    fn check(&self) -> Result<()> {
        // Another thread (none runs a query's operators today) is not measured.
        if std::thread::current().id() != self.thread {
            return Ok(());
        }
        let used = graphbuff_core::accounting::thread_bytes().wrapping_sub(self.start);
        if used > 0 && used as u64 > self.limit {
            return Err(memory_exceeded(self.limit));
        }
        Ok(())
    }
}

impl Interrupt {
    /// Starts the clock of `options` now; `None` if they set no limit.
    pub fn start(options: &QueryOptions) -> Option<Interrupt> {
        options.is_limited().then(|| Interrupt {
            deadline: options.timeout.map(|t| (Instant::now() + t, t)),
            cancel: options.cancel.clone(),
            memory: options.max_memory.filter(|_| graphbuff_core::accounting::is_counting()).map(|limit| {
                MemoryLimit {
                    thread: std::thread::current().id(),
                    start: graphbuff_core::accounting::thread_bytes(),
                    limit,
                }
            }),
            ticks: Cell::new(0),
        })
    }

    /// Fails if the query was cancelled, ran out of time or holds too much memory. Cheap:
    /// looks at the memory every [`MEMORY_EVERY`] calls and reads the clock every
    /// [`CLOCK_EVERY`].
    #[inline]
    pub fn check(&self) -> Result<()> {
        let t = self.ticks.get().wrapping_add(1);
        self.ticks.set(t);
        if !t.is_multiple_of(MEMORY_EVERY) {
            return Ok(());
        }
        if !t.is_multiple_of(CLOCK_EVERY) {
            return self.memory.as_ref().map_or(Ok(()), MemoryLimit::check);
        }
        self.check_now()
    }

    /// [`Interrupt::check`], always reading the clock and the memory.
    pub fn check_now(&self) -> Result<()> {
        if let Some(c) = &self.cancel
            && c.is_cancelled()
        {
            return Err(cancelled());
        }
        if let Some((deadline, limit)) = self.deadline
            && Instant::now() >= deadline
        {
            return Err(timed_out(limit));
        }
        if let Some(m) = &self.memory {
            m.check()?;
        }
        Ok(())
    }

    /// When the time limit passes, if there is one.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline.map(|(d, _)| d)
    }
}

pub(crate) fn cancelled() -> Error {
    Error::Cancelled("the query was cancelled".into())
}

pub(crate) fn memory_exceeded(limit: u64) -> Error {
    Error::MemoryLimit(format!(
        "the query used more memory than its limit of {} MiB",
        limit.div_ceil(1 << 20)
    ))
}

pub(crate) fn timed_out(limit: Duration) -> Error {
    Error::Timeout(format!("the query ran longer than its time limit of {} ms", limit.as_millis()))
}
