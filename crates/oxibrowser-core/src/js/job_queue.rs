//! Custom JobQueue for boa_engine that enables async operations and timer support.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

use boa_engine::JsResult;
use boa_engine::context::Context;
use boa_engine::job::{Job, JobExecutor};

/// Entry in the timer list (sorted by deadline).
#[derive(Debug, Clone)]
pub struct TimerEntry {
    pub deadline: Instant,
    pub id: u64,
    pub is_interval: bool,
    pub callback: boa_engine::JsObject,
    pub args: Vec<boa_engine::JsValue>,
    pub interval_ms: Option<u64>,
}

/// A custom JobQueue for boa_engine that supports timers and async operations.
#[derive(Debug)]
pub struct TokioJobQueue {
    microtasks: RefCell<VecDeque<Job>>,
    timers: RefCell<Vec<TimerEntry>>,
    next_timer_id: RefCell<u64>,
}

impl TokioJobQueue {
    pub fn new() -> Self {
        Self {
            microtasks: RefCell::new(VecDeque::new()),
            timers: RefCell::new(Vec::new()),
            next_timer_id: RefCell::new(1),
        }
    }

    /// Returns the next timer deadline, if any timers are scheduled.
    pub fn next_timer_deadline(&self) -> Option<Instant> {
        self.timers.borrow().first().map(|t| t.deadline)
    }

    /// Pop all timers whose deadline has passed.
    pub fn pop_due_timers(&self) -> Vec<TimerEntry> {
        let now = Instant::now();
        self.timers
            .borrow_mut()
            .extract_if(.., |t| t.deadline <= now)
            .collect()
    }

    /// Schedule a new timer. Returns the timer ID.
    pub fn schedule_timer(
        &self,
        deadline: Instant,
        callback: boa_engine::JsObject,
        args: Vec<boa_engine::JsValue>,
        is_interval: bool,
        interval_ms: Option<u64>,
    ) -> u64 {
        let id = *self.next_timer_id.borrow();
        *self.next_timer_id.borrow_mut() += 1;

        let entry = TimerEntry {
            deadline,
            id,
            is_interval,
            callback,
            args,
            interval_ms,
        };

        self.timers.borrow_mut().push(entry);
        // Sort by deadline ascending
        self.timers.borrow_mut().sort_by_key(|t| t.deadline);
        id
    }

    /// Cancel a timer by ID.
    pub fn cancel_timer(&self, timer_id: u64) -> bool {
        let mut timers = self.timers.borrow_mut();
        let len_before = timers.len();
        timers.retain(|t| t.id != timer_id);
        timers.len() != len_before
    }

    /// Clear all timers (used on context reset).
    pub fn clear_all_timers(&self) {
        self.timers.borrow_mut().clear();
    }

    /// Number of pending timers.
    pub fn timer_count(&self) -> usize {
        self.timers.borrow().len()
    }

    /// Number of pending microtasks.
    pub fn microtask_count(&self) -> usize {
        self.microtasks.borrow().len()
    }

    /// Host automation pump: callbacks cannot extend this queue without bound.
    pub(crate) fn run_bounded_jobs(
        &self,
        context: &mut Context,
        limit: usize,
    ) -> Result<usize, ()> {
        let mut count = 0;
        loop {
            if count == limit && !self.microtasks.borrow().is_empty() {
                return Err(());
            }
            let Some(job) = self.microtasks.borrow_mut().pop_front() else {
                return Ok(count);
            };
            if is_async_job(&job) {
                // Async continuations cannot be pumped inline (they need the
                // cooperative executor); requeue and signal the pump to stop.
                self.microtasks.borrow_mut().push_front(job);
                return Err(());
            }
            count += 1;
            call_sync_job(job, context).map_err(|_| ())?;
        }
    }
}

impl Default for TokioJobQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Run one synchronous job.
///
/// `setTimeout`/`setInterval` are implemented natively through
/// `schedule_timer`, so engine-level clock jobs are not expected; run them
/// inline as a fallback rather than dropping them.
fn call_sync_job(job: Job, context: &mut Context) -> JsResult<()> {
    match job {
        Job::PromiseJob(j) => j.call(context).map(|_| ()),
        Job::GenericJob(j) => j.call(context).map(|_| ()),
        // setTimeout/setInterval are implemented natively through
        // `schedule_timer`, so engine-level clock jobs are not expected;
        // run them inline as a fallback rather than dropping them.
        Job::TimeoutJob(j) => j.call(context).map(|_| ()),
        Job::IntervalJob(j) => j.call(context).map(|_| ()),
        other => unreachable!("async job kinds must be routed to run_jobs_async: {other:?}"),
    }
}

/// True for job kinds that poll to completion only through the cooperative
/// drain; blocking on them inline deadlocks (measured: async continuations
/// awaiting promise jobs, 2026-10-24).
fn is_async_job(job: &Job) -> bool {
    matches!(job, Job::AsyncJob(_))
}

impl JobExecutor for TokioJobQueue {
    fn enqueue_job(self: Rc<Self>, job: Job, _context: &mut Context) {
        match job {
            // Cleanup jobs pend until the engine signals — which this sync
            // executor never does. Drop them (cleanup callbacks do not run);
            // polling them would burn the drain's pass budget on every page
            // that merely constructs a FinalizationRegistry.
            Job::FinalizationRegistryCleanupJob(_) => {
                tracing::debug!("FinalizationRegistry cleanup job dropped (sync executor)");
            }
            job => self.microtasks.borrow_mut().push_back(job),
        }
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        self.drain_jobs(context)
    }
}

impl TokioJobQueue {
    /// Drain all queued jobs: synchronous kinds run to completion; async
    /// kinds (NativeAsyncJob continuations) are polled cooperatively between
    /// sync drains with a no-op waker, boa `SimpleJobExecutor`-style.
    ///
    /// Two deadlock classes live here, both measured on the boa 0.22
    /// upgrade (2026-10-24):
    /// - blocking on one async future deadlocks when it awaits a sync job;
    /// - awaiting the poll suspends forever when the future pends on an
    ///   external event.
    ///
    /// Hence: non-blocking poll + no-op waker + pass cap. Past the cap,
    /// incomplete futures are dropped (continuation lost — the same visible
    /// outcome as the pre-upgrade hang, minus the hang).
    fn drain_jobs(self: &Rc<Self>, context: &mut Context) -> JsResult<()> {
        use futures::stream::{FuturesUnordered, StreamExt};
        let cell = RefCell::new(context);
        let mut async_jobs: FuturesUnordered<_> = FuturesUnordered::new();
        let mut passes = 0u32;

        loop {
            // Classify whatever enqueue_job queued since the last pass.
            let queued: Vec<Job> = self.microtasks.borrow_mut().drain(..).collect();
            for job in queued {
                if is_async_job(&job) {
                    let Job::AsyncJob(a) = job else {
                        unreachable!("is_async_job guarantees AsyncJob here");
                    };
                    async_jobs.push(Box::pin(a.call(&cell)));
                } else if let Err(err) = call_sync_job(job, *cell.borrow_mut()) {
                    self.microtasks.borrow_mut().clear();
                    return Err(err);
                }
            }

            if self.microtasks.borrow().is_empty() && async_jobs.is_empty() {
                return Ok(());
            }

            passes += 1;
            if passes > 1_000 {
                tracing::debug!(
                    pending = async_jobs.len(),
                    "async job drain pass cap reached; dropping incomplete continuations"
                );
                return Ok(());
            }

            // One non-blocking poll of the async jobs; anything they enqueue
            // (promise jobs, more async jobs) is drained on the next pass.
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            let mut next = async_jobs.next();
            if let std::task::Poll::Ready(Some(Err(err))) =
                std::pin::Pin::new(&mut next).poll(&mut cx)
            {
                self.microtasks.borrow_mut().clear();
                return Err(err);
            }
        }
    }
}
