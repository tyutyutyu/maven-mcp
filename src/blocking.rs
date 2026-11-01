//! Bounded, cancellable execution of the blocking index, archive and file work behind the
//! tools, so that heavy requests never occupy the async runtime that serves the protocol.
//!
//! A job first reserves one admission slot (`running + queued` in total) and is rejected
//! immediately when none is free. It then waits for one of the running slots and only then
//! starts on a blocking thread. Both slots are released when the blocking function itself
//! returns, even if the request was cancelled meanwhile, so the number of threads and the
//! amount of abandoned work stay bounded.
//!
//! A running closure cannot be interrupted from outside. Cancelling the request (dropping the
//! future) or shutting the server down cancels the job's token, and the long loops of the
//! closure observe it through [`checkpoint`] and [`is_cancelled`].

use std::{cell::RefCell, error::Error, fmt, sync::Arc};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

thread_local! {
    static CURRENT: RefCell<Option<CancellationToken>> = const { RefCell::new(None) };
}

/// The work observed a cancelled job at a checkpoint and stopped; its partial result must
/// be discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("blocking work was cancelled")
    }
}

impl Error for Cancelled {}

/// Whether the job running on this thread was cancelled. Always false outside [`scope`].
pub fn is_cancelled() -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    })
}

/// Cancellation checkpoint for long loops: fails once the job running on this thread was
/// cancelled. It never fails outside [`scope`].
pub fn checkpoint() -> Result<(), Cancelled> {
    if is_cancelled() {
        Err(Cancelled)
    } else {
        Ok(())
    }
}

struct RestoreCurrent(Option<CancellationToken>);

impl Drop for RestoreCurrent {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.0.take());
    }
}

/// Runs `work` on this thread with `token` as the job whose cancellation [`checkpoint`] observes.
pub fn scope<T>(token: &CancellationToken, work: impl FnOnce() -> T) -> T {
    let previous = CURRENT.with(|current| current.borrow_mut().replace(token.clone()));
    let _restore = RestoreCurrent(previous);
    work()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeavyWorkLimits {
    /// Jobs allowed to execute at the same time.
    pub running: usize,
    /// Further jobs allowed to wait for a running slot.
    pub queued: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeavyWorkError {
    /// Every running and queued slot is taken; the job was not accepted.
    Overloaded(HeavyWorkLimits),
    /// The server is shutting down; the job did not start or its result was discarded.
    Cancelled,
    /// The blocking function panicked.
    Failed,
}

impl fmt::Display for HeavyWorkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overloaded(limits) => write!(
                formatter,
                "the server is overloaded: {} heavy jobs are running and {} are queued (MAX_HEAVY_JOBS and MAX_QUEUED_HEAVY_JOBS); retry later",
                limits.running, limits.queued
            ),
            Self::Cancelled => formatter.write_str("the request was cancelled"),
            Self::Failed => formatter.write_str("blocking work failed unexpectedly"),
        }
    }
}

impl Error for HeavyWorkError {}

#[derive(Debug)]
struct Slots {
    limits: HeavyWorkLimits,
    running: Arc<Semaphore>,
    admitted: Arc<Semaphore>,
    shutdown: CancellationToken,
}

/// The shared pool of heavy jobs of one server.
#[derive(Debug, Clone)]
pub struct HeavyWork {
    slots: Arc<Slots>,
}

impl HeavyWork {
    pub fn new(limits: HeavyWorkLimits) -> Self {
        Self {
            slots: Arc::new(Slots {
                limits,
                running: Arc::new(Semaphore::new(limits.running)),
                admitted: Arc::new(Semaphore::new(limits.running.saturating_add(limits.queued))),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    pub fn limits(&self) -> HeavyWorkLimits {
        self.slots.limits
    }

    /// Running plus queued jobs, including cancelled jobs whose blocking function has not
    /// returned yet.
    pub fn active_jobs(&self) -> usize {
        let capacity = self
            .slots
            .limits
            .running
            .saturating_add(self.slots.limits.queued);
        capacity.saturating_sub(self.slots.admitted.available_permits())
    }

    /// Cancels every job; running closures stop at their next checkpoint and no new job starts.
    pub fn shutdown(&self) {
        self.slots.shutdown.cancel();
    }

    /// Runs `work` on a blocking thread once a running slot is free. Dropping the returned
    /// future cancels the job; its result is never delivered after cancellation.
    pub async fn run<T, F>(&self, work: F) -> Result<T, HeavyWorkError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let slots = &self.slots;
        let admitted = Arc::clone(&slots.admitted)
            .try_acquire_owned()
            .map_err(|_| HeavyWorkError::Overloaded(slots.limits))?;
        let token = slots.shutdown.child_token();
        let cancel_on_drop = token.clone().drop_guard();
        let running = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(HeavyWorkError::Cancelled),
            permit = Arc::clone(&slots.running).acquire_owned() => {
                permit.map_err(|_| HeavyWorkError::Failed)?
            }
        };
        let job = token.clone();
        // The request's span (its tool call) also covers the events of its blocking work.
        let span = tracing::Span::current();
        let handle = tokio::task::spawn_blocking(move || {
            let _entered = span.entered();
            let _slots = (admitted, running);
            scope(&job, || (!job.is_cancelled()).then(work))
        });
        let outcome = handle.await;
        cancel_on_drop.disarm();
        match outcome {
            Ok(Some(value)) if !token.is_cancelled() => Ok(value),
            Ok(_) => Err(HeavyWorkError::Cancelled),
            Err(_) => Err(HeavyWorkError::Failed),
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/blocking.rs"]
mod tests;
