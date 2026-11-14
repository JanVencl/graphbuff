//! Query timeouts and cancellation.
//!
//! A query runs with [`QueryOptions`]: an optional time limit and an optional
//! [`CancelHandle`]. The executor checks them while it works (once per row produced or
//! candidate looked at, reading the clock every few hundred checks), so a long query
//! stops soon after its limit passes or it is cancelled, and fails with
//! [`Error::Timeout`] or [`Error::Cancelled`]. A writing query that stops is rolled back
//! like any failed query; in an explicit transaction the whole transaction rolls back.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
pub struct CancelHandle(Arc<AtomicBool>);

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the queries running with this handle to stop, and makes later queries with
    /// it fail at once. Cannot be undone.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
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

    /// Whether these options limit the query at all.
    pub(crate) fn is_limited(&self) -> bool {
        self.timeout.is_some() || self.cancel.is_some()
    }
}

/// How many checks pass between two reads of the clock.
const CLOCK_EVERY: u32 = 256;

/// The limits of a running query, checked by the executor.
#[derive(Debug)]
pub(crate) struct Interrupt {
    deadline: Option<(Instant, Duration)>,
    cancel: Option<CancelHandle>,
    ticks: Cell<u32>,
}

impl Interrupt {
    /// Starts the clock of `options` now; `None` if they set no limit.
    pub fn start(options: &QueryOptions) -> Option<Interrupt> {
        options.is_limited().then(|| Interrupt {
            deadline: options.timeout.map(|t| (Instant::now() + t, t)),
            cancel: options.cancel.clone(),
            ticks: Cell::new(0),
        })
    }

    /// Fails if the query was cancelled or ran out of time. Cheap: reads the clock only
    /// every [`CLOCK_EVERY`] calls.
    #[inline]
    pub fn check(&self) -> Result<()> {
        let t = self.ticks.get().wrapping_add(1);
        self.ticks.set(t);
        if !t.is_multiple_of(CLOCK_EVERY) {
            return Ok(());
        }
        self.check_now()
    }

    /// [`Interrupt::check`], always reading the clock.
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

pub(crate) fn timed_out(limit: Duration) -> Error {
    Error::Timeout(format!("the query ran longer than its time limit of {} ms", limit.as_millis()))
}
