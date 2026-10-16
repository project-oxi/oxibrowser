//! Custom JobQueue for boa_engine that enables async operations and timer support.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::Instant;

use boa_engine::context::Context;
use boa_engine::job::{JobQueue, NativeJob};

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
    microtasks: RefCell<VecDeque<NativeJob>>,
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
            count += 1;
            job.call(context).map_err(|_| ())?;
        }
    }
}

impl Default for TokioJobQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl JobQueue for TokioJobQueue {
    fn enqueue_promise_job(&self, job: NativeJob, _context: &mut Context) {
        self.microtasks.borrow_mut().push_back(job);
    }

    fn run_jobs(&self, context: &mut Context) {
        // We must release the RefCell borrow between iterations so that
        // job callbacks can enqueue new microtasks without panicking.
        loop {
            let job = self.microtasks.borrow_mut().pop_front();
            match job {
                Some(job) => {
                    if job.call(context).is_err() {
                        self.microtasks.borrow_mut().clear();
                        return;
                    }
                }
                None => return,
            }
        }
    }

    fn enqueue_future_job(&self, future: boa_engine::job::FutureJob, context: &mut Context) {
        // For now, just poll synchronously (blocks JS thread)
        // Future work: integrate with tokio for true async
        let job = pollster::block_on(future);
        self.enqueue_promise_job(job, context);
    }
}
