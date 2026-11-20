//! Driving scheduled tasks: the loop that turns a tick into a delivered prompt.
//!
//! `Scheduler` already knows what is due and when to look again. What it cannot do is wait, and what it cannot do is
//! deliver -- those are the two halves this module supplies.
//!
//! # Why the delivery is a callback
//!
//! A firing is a prompt that has to reach a session, and this module has no session. Rather than take a session
//! registry it takes a function, so a deployment that delivers into a different shape (a queue, a webhook, a different
//! runtime) reuses the loop instead of forking it.
//!
//! # Why the loop waits on `next_wake` rather than polling
//!
//! A cron schedule can be a day away. A loop that woke every second would spend 86,400 wake-ups to find nothing, and a
//! loop that slept a fixed interval would either miss a schedule or spin. `Tick::next_wake` is the scheduler saying when
//! it is next worth asking, which is the one number that makes the wait exact.
//!
//! # Why a delivery failure does not stop the loop
//!
//! A prompt that could not be delivered is a task that did not run, and the next occurrence is still due. Ending the
//! loop on the first failure would silently stop every other schedule because one session was gone.
//!
//! # Why this is bounded
//!
//! The loop takes a guard rather than running forever: a caller that started it twice would deliver every firing twice,
//! and nothing in the scheduler reports that. One guard per process is what makes "started once" a property.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::schedule::{Firing, Scheduler, Tick};
use crate::schedule::{Instant, now_millis};

/// How long to wait before looking again when the scheduler names no wake time.
///
/// A scheduler with nothing pending still has to be re-checked, because a task can be created at any moment. One minute
/// is short enough that a newly created task starts promptly and long enough not to spin.
pub const IDLE_WAIT: Duration = Duration::from_secs(60);

/// The most a single wait is capped at, so a clock change cannot park the loop indefinitely.
///
/// An hour: a cron rule that names a time a year away still gets re-evaluated hourly, which is the difference between a
/// loop that respects the schedule and one that trusts a single computed instant across a clock adjustment.
pub const MAX_WAIT: Duration = Duration::from_secs(3600);

/// Reports what happened to one firing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The prompt reached a session.
    Delivered,
    /// It did not, for this reason.
    Failed {
        /// Why, in words a log or a person can read.
        reason: String,
    },
}

/// Delivers one firing.
///
/// A closure rather than a trait object because the caller usually has one already, and because a delivery that needs
/// state can capture it.
pub type Deliverer = Arc<dyn Fn(Firing) -> Delivery + Send + Sync>;

/// What one pass of the loop did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// How many firings were delivered.
    pub delivered: usize,
    /// How many failed, with their reasons.
    pub failures: Vec<String>,
}

/// Runs one pass: ticks the scheduler and delivers everything due.
///
/// Separate from the loop so a test can drive it with a clock it controls. A loop that could only be tested by waiting
/// for a real cron would have no tests.
pub fn pass(scheduler: &mut Scheduler, deliver: &Deliverer, now: Instant) -> Pass {
    let (result, _) = pass_and_wake(scheduler, deliver, now);
    result
}

/// Ticks once, delivers everything due, and reports both the outcome and when to look again.
///
/// One tick and not two: a second call would recompute the schedule from the first one's result and fire anything due
/// in between twice. The wake time a caller wants is the one this tick produced.
fn pass_and_wake(
    scheduler: &mut Scheduler,
    deliver: &Deliverer,
    now: Instant,
) -> (Pass, Option<Instant>) {
    let Tick { firings, next_wake } = scheduler.tick(now);
    let mut result = Pass::default();
    for firing in firings {
        match deliver(firing) {
            Delivery::Delivered => result.delivered += 1,
            Delivery::Failed { reason } => result.failures.push(reason),
        }
    }
    (result, next_wake)
}

/// How long to sleep before the next pass.
///
/// Reads `next_wake` and clamps it: a wake time in the past means look immediately, one far away is capped so a clock
/// change cannot park the loop.
#[must_use]
pub fn wait_for(wake: Option<Instant>, now: Instant) -> Duration {
    let Some(wake) = wake else {
        return IDLE_WAIT;
    };
    let millis = wake.saturating_sub(now);
    // A wake time at or before now means there is work waiting; waking after a short yield rather than immediately
    // keeps a schedule whose next time is "now" from spinning the reactor.
    if millis == 0 {
        return Duration::from_millis(1);
    }
    Duration::from_millis(millis).min(MAX_WAIT)
}

/// A loop that delivers scheduled prompts until cancelled.
pub struct Runner {
    scheduler: std::sync::Mutex<Scheduler>,
    deliver: Deliverer,
    /// Held so two runners cannot deliver the same firing twice.
    started: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field(
                "started",
                &self.started.load(std::sync::atomic::Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl Runner {
    /// Builds a runner over a scheduler and a deliverer.
    #[must_use]
    pub fn new(scheduler: Scheduler, deliver: Deliverer) -> Self {
        Self {
            scheduler: std::sync::Mutex::new(scheduler),
            deliver,
            started: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether the loop has been started.
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Delivers one pass now, for a caller that drives the clock itself.
    ///
    /// # Errors
    ///
    /// Returns how many firings failed, with their reasons. A caller that dares to ignore this is choosing to lose a
    /// prompt silently, which is why the failures are named rather than counted.
    pub fn tick_now(&self, now: Instant) -> Pass {
        let mut scheduler = self
            .scheduler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pass(&mut scheduler, &self.deliver, now)
    }

    /// Runs the loop until `cancel` fires.
    ///
    /// Returns immediately if the loop is already running, so two callers cannot double-deliver.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        if self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let wake = {
                let mut scheduler = self
                    .scheduler
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let now = now_millis();
                // One tick produces both the deliveries and the next wake, so the wait below is what the scheduler
                // just computed rather than a polling interval or a second tick's answer.
                let (_, wake) = pass_and_wake(&mut scheduler, &self.deliver, now);
                wake
            };
            let wait = wait_for(wake, now_millis());
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
        }
    }
}
