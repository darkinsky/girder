//! A fixed-capacity admission pool for subagents.
//!
//! Upstream `@deepseek-ai/dsh-subagent` gates child activations with `ActivationPool`
//! (`packages/subagent/subagent/src/activation.ts`): a `Set` of symbol slots shared by every member
//! of a subagent tree, sized by the `maxActiveSubagents` configuration (default 8, minimum 1),
//! reserved in `SubagentManager` before a child is reconstructed. Reserving an activation past the
//! limit throws `ACTIVATION_LIMIT_REACHED` instead of queueing, so a runaway fan-out fails loudly
//! and the model is told to wait or finish the work itself.
//!
//! Girder's [`SubagentRegistry`](crate::SubagentRegistry) has no counterpart: [`record_start`] is a
//! bookkeeping call that always succeeds and always inserts. Everything a caller spawns, therefore,
//! runs at once. One tool call that fans out to a hundred children creates a hundred concurrent
//! agent loops — a hundred provider streams, a hundred sets of process-wide rate limits split a
//! hundred ways, and a hundred live sessions to keep in memory — with nothing anywhere that can say
//! "not now". The registry would faithfully record all hundred as `running`, which makes the
//! resulting slowness look like a provider problem rather than the saturation it is.
//!
//! This module is the missing bound, deliberately kept as a standalone primitive: the registry
//! describes *what was spawned* and must keep doing so even for a refused spawn, while admission is
//! a separate decision that a caller can make before it touches the registry.
//!
//! [`record_start`]: crate::SubagentRegistry::record_start
//!
//! # Why a permit, not a counter
//!
//! The count of running children and the number of free places are the same fact, so this type
//! stores it once — as permits in a [`tokio::sync::Semaphore`] — and derives [`Pool::active`] from
//! the permits still available. A separate `running: AtomicUsize` incremented on acquire and
//! decremented on release would be a second copy of that fact, and the classic way to break an
//! admission pool is for the two copies to disagree: a release path that forgets to decrement leaks
//! capacity forever, and one that double-decrements admits more children than the bound allows.
//! With a permit there is no decrement to forget; dropping the permit *is* the release.
//!
//! # Why releasing consumes the slot
//!
//! [`Pool::release`] takes [`Slot`] by value, and `Slot` is neither `Copy` nor `Clone`. Releasing
//! the same slot twice is therefore a compile error, not a runtime hazard. The alternative — an
//! idempotent release that looks the slot up in a set of live slots — moves the bug from the type
//! system to a runtime check that can only fail *after* the accounting has already been corrupted
//! (which is exactly when it is hardest to diagnose). Upstream makes the same trade: its reserved
//! slot is released by an opaque closure, and calling that closure twice happens to be safe because
//! the `Set` delete is idempotent, but nothing stops a caller from doing it.
//!
//! Dropping a `Slot` without calling [`Pool::release`] also releases it, because the permit's own
//! `Drop` returns it. That matters for cancellation: a task holding a slot that is aborted mid-run
//! must not permanently consume capacity. `release` still exists so the normal path names the moment
//! capacity is handed back; it is not a correctness requirement.
//!
//! # Why capacity zero is refused
//!
//! A pool with capacity zero is a pool that can never admit anything: [`Pool::try_acquire`] always
//! returns `None` and [`Pool::acquire`] waits forever for a permit no release can create. That is
//! not a bound on concurrency, it is a deadlock with a friendly name, and a caller that asks for it
//! has made a configuration error — upstream's schema enforces the same minimum of 1. The
//! constructor panics rather than accepting a pool that silently does nothing, matching
//! `BoundedQueue::new`, which refuses zero for the same reason. A caller that wants "refuse
//! everything" should skip the pool instead of hiding the decision inside a bound that reports
//! itself as a pool.
//!
//! # Fairness
//!
//! [`Pool::acquire`] inherits [`tokio::sync::Semaphore`]'s FIFO wakeup order: waiters are served in
//! the order they started waiting, not in the order they happen to be scheduled. This is why the
//! semaphore is used here rather than a hand-rolled retry loop around [`Pool::try_acquire`] — a
//! `try_acquire` retry loop lets any sender that wakes up first win, so one busy caller can starve
//! everyone else indefinitely.
//!
//! # Example
//!
//! ```text
//! let pool = Pool::new(2);
//! let slot = pool.try_acquire().expect("room for two"); // or: pool.acquire().await
//! assert_eq!(pool.active(), 1);
//! pool.release(slot);
//! assert_eq!(pool.active(), 0);
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// One subagent task's claim on a place in the pool.
///
/// A slot is produced only by [`Pool::try_acquire`] or [`Pool::acquire`], carries the permit that
/// keeps the place reserved, and is consumed by [`Pool::release`]. It is intentionally neither
/// `Copy` nor `Clone`: transferring the value transfers the obligation, so two owners can never
/// believe they hold the same place.
#[derive(Debug)]
pub struct Slot {
    /// Which acquisition this is, in order. Only for diagnostics and log lines: a permit has no
    /// identity of its own, and "slot 7 is still running" is far easier to read in a trace than
    /// "some slot is still running".
    index: usize,
    /// Keeping the permit alive is what keeps the place reserved. `Option` so that [`Pool::release`]
    /// can hand it back before the slot is dropped, keeping the documented release point exact
    /// rather than "whenever the destructor runs".
    permit: Option<OwnedSemaphorePermit>,
}

impl Slot {
    /// The order in which this slot was acquired, counting from zero for each pool.
    ///
    /// Stable for the life of the slot and never reused, so a log line that mentions it identifies
    /// one acquisition rather than a position.
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }
}

/// A fixed-capacity pool of subagent slots.
///
/// Cheap to share: clone the surrounding [`Arc`] to hand the same bound to several call sites, or
/// put a single `Pool` behind a reference. Every method takes `&self`, so a shared pool needs no
/// lock of its own — the semaphore is the synchronisation.
#[derive(Debug)]
pub struct Pool {
    /// The bound, stored so that [`Pool::capacity`] and [`Pool::active`] do not have to remember it
    /// separately from the permits.
    capacity: usize,
    /// The admission decision itself. `Arc` because waiting for a permit must not borrow the pool:
    /// an owned wait can be driven from any task and survives the caller's borrow ending.
    permits: Arc<Semaphore>,
    /// Source of [`Slot::index`]. Monotonic and only for diagnostics; it is not the live count.
    next_index: AtomicUsize,
}

impl Pool {
    /// Creates a pool that admits at most `capacity` subagents at a time.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero. Such a pool could never admit anything — see the module
    /// documentation — so accepting it would turn a configuration mistake into a hang.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "a subagent pool needs room for at least one child; a pool that can never admit \
             anything is a deadlock, not a bound"
        );
        Self {
            capacity,
            permits: Arc::new(Semaphore::new(capacity)),
            next_index: AtomicUsize::new(0),
        }
    }

    /// Takes a place if one is free, and returns `None` immediately if none is.
    ///
    /// This is the call for a design that must not queue: the caller can refuse the delegation and
    /// tell the model to wait for an existing child, which is what upstream's `reserve` does by
    /// throwing. Waiting is [`Pool::acquire`]'s job.
    #[must_use]
    pub fn try_acquire(&self) -> Option<Slot> {
        let permit = self.permits.clone().try_acquire_owned().ok()?;
        Some(self.slot(permit))
    }

    /// Takes a place, waiting for one if the pool is full.
    ///
    /// Waiters are served first-come, first-served, so a burst of callers cannot starve an earlier
    /// one. The wait cannot fail: nothing in this module closes the semaphore, and the pool exposes
    /// no operation that could.
    pub async fn acquire(&self) -> Slot {
        // `AcquireError` means the semaphore was closed, and only `Semaphore::close` closes it, which
        // this type never calls — so the error arm is unreachable. It is written as a panic with a
        // reason rather than an `unwrap` so that a future change that does close the pool reports why
        // the invariant broke instead of pointing at an anonymous `unwrap`.
        let Ok(permit) = self.permits.clone().acquire_owned().await else {
            unreachable!("the subagent pool never closes its semaphore")
        };
        self.slot(permit)
    }

    /// Returns a place to the pool, waking the longest-waiting caller if any.
    ///
    /// Takes the slot by value: a slot cannot be released twice because the second call has no slot
    /// to pass. It does not have to be released through the pool that produced it — the permit owns
    /// its semaphore — but releasing it anywhere is what makes the place available again.
    ///
    /// Releasing a slot twice does not compile: [`Slot`] is neither `Copy` nor `Clone`, so the second
    /// call has no value to pass (`error[E0382]: use of moved value`). That is deliberately not a
    /// runtime check — a check can only fire after the accounting is already wrong, and a test for it
    /// could only assert a panic the type system makes unreachable.
    #[allow(
        clippy::unused_self,
        reason = "release is a method on the pool by design: a caller says which pool it is \
                  returning the place to, and the call site reads as one operation even though the \
                  permit itself owns the semaphore"
    )]
    pub fn release(&self, slot: Slot) {
        // Halting the permit before dropping the slot makes the release point this statement
        // instead of the end of the caller's scope, so the next `active()` agrees with the fact
        // that the child is done.
        drop(slot.permit);
    }

    /// How many subagents are running right now.
    ///
    /// Derived from the permits still available rather than from a separate counter, so it cannot
    /// drift away from the admission decision.
    #[must_use]
    pub fn active(&self) -> usize {
        self.capacity
            .saturating_sub(self.permits.available_permits())
    }

    /// The bound this pool was created with.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether at least one place is free right now.
    ///
    /// Advisory: another caller can take the place before the caller acts on the answer. It exists
    /// so that a caller can avoid building a request it would only have to throw away, not as a
    /// substitute for [`Pool::try_acquire`].
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.permits.available_permits() > 0
    }

    /// Wraps a freshly acquired permit as a numbered slot.
    fn slot(&self, permit: OwnedSemaphorePermit) -> Slot {
        let index = self.next_index.fetch_add(1, Ordering::Relaxed);
        Slot {
            index,
            permit: Some(permit),
        }
    }
}
