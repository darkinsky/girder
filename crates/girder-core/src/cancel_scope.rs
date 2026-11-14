//! Cancellation scopes: a tree whose cancellation flows from a parent to every descendant.
//!
//! # Why a second cancellation primitive
//!
//! `tokio_util::sync::CancellationToken` already links a parent to its children, and the loop in
//! `agent_loop` checks one token before every step and forwards it into the provider. What that
//! token cannot say is *why* the work stopped: a cancelled turn, a cancelled subagent
//! (`subagent::SubagentError::Cancelled`) and a cancelled background job all read as the single
//! word "cancelled", so an operator starting from a stopped session cannot tell a person's
//! interrupt (`runtime::AgentRuntime::cancel`) from an expired tool deadline. [`CancelReason`] is
//! that missing record.
//!
//! The other gap is what to undo. A token stops work; it does not say what a stopped piece of work
//! should do with what it already produced. `agent_loop` yields its steps as they arrive and then
//! returns `LoopError::Cancelled` instead of a truncated success, which keeps the session log
//! honest, but the *owner* of the cancelled work still has to release what it holds — a subagent
//! registry slot, a half-written deliverable, a follow-up task. [`Scope::on_cancel`] is where that
//! action goes, run exactly once.
//!
//! # Why `Parent` is a reason of its own
//!
//! A child that stopped because its parent stopped was never asked to stop. Collapsing the two
//! makes the first question of every cancellation investigation — "which scope started this?" —
//! unanswerable, and reports the user's Ctrl-C as though they had aimed it at one subagent. Keeping
//! [`CancelReason::Parent`] distinct means a cancelled grandchild says the cancellation came from
//! above, and the scope that carries [`CancelReason::UserRequest`] identifies the owner.
//!
//! # Why propagation is synchronous and one-way
//!
//! Cancelling a scope walks its live descendants and marks each one before `cancel` returns. The
//! alternative — one task or thread per child awaiting the parent's token — needs a spawn that can
//! fail, outlives the tree it watches, and leaves `is_cancelled` false for an unbounded moment
//! after `cancel`. A walk cannot fail and makes the whole subtree observably cancelled at a single
//! point in time.
//!
//! A child created *after* an ancestor was cancelled is marked at creation. The child is registered
//! with its parent *before* that check, so a concurrent `cancel` on the parent either observes the
//! new child during its walk or is observed by the child's check; registering afterwards would
//! leave a window in which neither happens and the child would start live under a cancelled parent.
//!
//! Propagation is deliberately one-way. Cancelling a child says only that this piece of work is
//! finished; the parent may still have other children and its own reason to run. A child that could
//! cancel its parent would make "stop one subagent" indistinguishable from "stop the session".
//!
//! # Why the first cause wins
//!
//! Cancellation usually arrives from more than one place at once — a user interrupt and a deadline
//! racing. The first reason recorded is kept, because a later one describes an event that changed
//! nothing: the work was already stopping. That is also why a second `cancel` is not an error and
//! why cleanup must not run twice.
//!
//! # Why cleanup is not in `Drop`
//!
//! [`run_cleanups`] is explicit for the same reason `scope::Scope::dispose` is: a cleanup deferred
//! to `Drop` runs during unwinding, at a moment the owner cannot choose, and a cleanup that touches
//! a session or a registry needs its owner to still be alive. Nor does dropping a [`Scope`] cancel
//! it; the tree is explicit about who stops what.
//!
//! # Example
//!
//! ```text
//! let root = Scope::root();
//! let child = root.child();
//! child.on_cancel(Box::new(|| eprintln!("child released its slot")));
//! root.cancel();                          // the child stops too
//! assert_eq!(child.reason(), Some(CancelReason::Parent));
//! run_cleanups(&root);                    // the child's action runs, once
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};

/// Identifies a scope, for diagnostics only.
type Id = u64;

/// Source of [`Id`]s.
///
/// Relaxed ordering is enough: the value is read when a [`std::fmt::Debug`] rendering is built, and
/// only uniqueness matters, never the order two scopes were created in.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Why a scope was cancelled.
///
/// One reason per cancellation, and the first one recorded is kept: a later cause describes a
/// cancellation that had already happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelReason {
    /// The user asked for the work to stop.
    ///
    /// The default for [`Scope::cancel`]. An interrupt that names no cause is a person's decision;
    /// guessing a timeout would send the next reader looking for a deadline that never existed.
    UserRequest,
    /// A deadline expired before the work finished.
    Timeout {
        /// How long the work was allowed to run, in milliseconds.
        after_ms: u64,
    },
    /// An ancestor scope was cancelled, taking this one with it.
    ///
    /// A separate reason rather than a copy of the ancestor's: the scope that stopped is not the
    /// scope that was asked to stop. Reporting "the user cancelled this subagent" when the user
    /// cancelled the session names the wrong owner and hides the scope that would need fixing.
    Parent,
    /// The work failed, and stopping is part of how it failed.
    Error {
        /// The failure, in the words of whoever produced it.
        detail: String,
    },
}

impl std::fmt::Display for CancelReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserRequest => formatter.write_str("the user asked for this to stop"),
            Self::Timeout { after_ms } => {
                write!(formatter, "the deadline expired after {after_ms} ms")
            }
            Self::Parent => formatter.write_str("the parent scope was cancelled"),
            Self::Error { detail } => write!(formatter, "cancelled by an error: {detail}"),
        }
    }
}

impl std::error::Error for CancelReason {}

/// The state one node of a cancellation tree owns, shared by every handle to that node.
///
/// `cancelled` is the only field read on the hot path ([`Scope::is_cancelled`] is checked before
/// every loop step), so it is an atomic rather than a lock. The reasons and the cleanup list are
/// written once or twice per scope and locked.
struct State {
    /// Told apart in a debug rendering, never used to decide anything.
    id: Id,
    /// Set by the first cancellation that reaches this node, whether its own or an ancestor's.
    cancelled: AtomicBool,
    /// The first cause recorded for this node, or `None` before it is cancelled.
    ///
    /// The mutex doubles as the [`State::signal`] guard, because the two are always written
    /// together: waiting for cancellation means waiting for this slot to become `Some`.
    reason: Mutex<Option<CancelReason>>,
    /// Wakes [`Scope::wait_until_cancelled`] when `cancelled` flips.
    signal: Condvar,
    /// Actions to run when this node is cancelled.
    cleanups: Mutex<Cleanups>,
    /// The node that cancels this one. `None` only for a root.
    ///
    /// Strong, so a `Scope` handle cannot outlive the ancestor whose cancellation it depends on.
    parent: Option<Arc<State>>,
    /// Descendants, weakest-first so a parent cannot keep a dropped child's state alive.
    ///
    /// Weak because the tree's lifetime runs the other way: a subagent may finish and be dropped
    /// long before the session that started it.
    children: Mutex<Vec<Weak<State>>>,
}

/// Cleanups registered on one node, and whether they have run.
#[derive(Default)]
struct Cleanups {
    /// Registered but not yet run, in registration order.
    actions: Vec<Box<dyn FnOnce() + Send>>,
    /// Set before the vector is drained, so a second [`run_cleanups`] finds it empty and a
    /// registration racing a drain either lands in the vector before it is drained or sees a
    /// cancelled scope and runs immediately. Without the flag, a cleanup could run twice.
    drained: bool,
}

impl State {
    /// A fresh, uncancelled node.
    fn new(parent: Option<Arc<State>>) -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            cancelled: AtomicBool::new(false),
            reason: Mutex::new(None),
            signal: Condvar::new(),
            cleanups: Mutex::new(Cleanups::default()),
            parent,
            children: Mutex::new(Vec::new()),
        }
    }

    /// The reason slot, ignoring poisoning.
    ///
    /// A panic in one cleanup must not make every later cancellation unreadable; the slot holds a
    /// plain `Option` and is never left half-written, so the inner value is sound.
    fn lock_reason(&self) -> MutexGuard<'_, Option<CancelReason>> {
        self.reason.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cleanup list, ignoring poisoning, for the same reason as [`State::lock_reason`].
    fn lock_cleanups(&self) -> MutexGuard<'_, Cleanups> {
        self.cleanups.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `reason` and marks this node cancelled, returning whether it was the first time.
    fn mark(&self, reason: CancelReason) -> bool {
        // The lock is held across the flag store and the notification so a waiter cannot miss the
        // wake-up: it either takes the lock before this block (and then sees the flag set) or is
        // inside `wait`, which released the lock atomically.
        let mut slot = self.lock_reason();
        let first = slot.is_none();
        if first {
            *slot = Some(reason);
        }
        self.cancelled.store(true, Ordering::SeqCst);
        self.signal.notify_all();
        first
    }

    /// Whether this node or any ancestor is cancelled.
    ///
    /// A loop rather than recursion, and it walks borrowed ancestors rather than cloning each
    /// `Arc`: the check runs before every loop step, and a cloned handle per level would be a
    /// refcount write on the hottest path in the tree.
    fn any_cancelled(self: &Arc<Self>) -> bool {
        let mut node = Some(self);
        while let Some(current) = node {
            if current.cancelled.load(Ordering::SeqCst) {
                return true;
            }
            node = current.parent.as_ref();
        }
        false
    }

    /// The live descendants of this node, pruning handles to dropped ones.
    ///
    /// Pruning here rather than in a `Drop` keeps a finished child's teardown off the cancellation
    /// path, and the walk that has to visit the list is the only place that would otherwise pay for
    /// the dead entries.
    fn live_children(&self) -> Vec<Arc<State>> {
        let mut children = self.children.lock().unwrap_or_else(PoisonError::into_inner);
        let live: Vec<Arc<State>> = children.iter().filter_map(Weak::upgrade).collect();
        children.retain(|weak| weak.strong_count() > 0);
        live
    }

    /// Registers `child` as a descendant of this node.
    fn adopt(&self, child: &Arc<State>) {
        let mut children = self.children.lock().unwrap_or_else(PoisonError::into_inner);
        children.retain(|weak| weak.strong_count() > 0);
        children.push(Arc::downgrade(child));
    }

    /// Cancels this node and every live descendant.
    ///
    /// The `first` check stops the walk early on a repeated cancellation: those descendants were
    /// already marked by the walk that did the work, and a child created since then was marked at
    /// creation.
    fn cancel_tree(self: &Arc<Self>, reason: CancelReason) {
        if !self.mark(reason) {
            return;
        }
        for child in self.live_children() {
            child.cancel_tree(CancelReason::Parent);
        }
    }

    /// Runs this node's cleanups if it is cancelled, then its descendants'.
    ///
    /// The vector is taken out and the lock released before any action runs: a cleanup that calls
    /// back into the scope (to read [`Scope::reason`], say) would otherwise deadlock on a lock this
    /// function still holds.
    fn run_cleanups_tree(self: &Arc<Self>) {
        let actions = {
            let mut cleanups = self.lock_cleanups();
            if cleanups.drained || !self.any_cancelled() {
                Vec::new()
            } else {
                cleanups.drained = true;
                std::mem::take(&mut cleanups.actions)
            }
        };
        for action in actions {
            action();
        }
        for child in self.live_children() {
            child.run_cleanups_tree();
        }
    }
}

/// A node in a cancellation tree.
///
/// Cloning is cheap and shares the node: every handle observes the same cancellation, and
/// [`Scope::on_cancel`] through any of them registers on the same cleanup list.
///
/// Handles are independent in what they can *do*, not in what they observe: cancelling any handle
/// to a node cancels that node, and the node's cancellation reaches every descendant.
#[derive(Clone)]
pub struct Scope {
    /// The shared node. Held in an `Arc` so children can keep their ancestors alive.
    state: Arc<State>,
}

impl std::fmt::Debug for Scope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Scope")
            .field("id", &self.state.id)
            .field("cancelled", &self.is_cancelled())
            .field("reason", &self.reason())
            .finish_non_exhaustive()
    }
}

impl Scope {
    /// Creates a root scope: cancelled only by a direct request.
    ///
    /// A root has no ancestor, so it can never receive [`CancelReason::Parent`] — the reason it
    /// records is always one somebody asked for.
    #[must_use]
    pub fn root() -> Self {
        Self {
            state: Arc::new(State::new(None)),
        }
    }

    /// Derives a child scope.
    ///
    /// The child is cancelled whenever its parent is, but the reverse does not hold; see the module
    /// documentation. A child created under an already-cancelled ancestor starts cancelled with
    /// [`CancelReason::Parent`], because work started in a stopped scope can never be allowed to
    /// run.
    #[must_use]
    pub fn child(&self) -> Self {
        let state = Arc::new(State::new(Some(Arc::clone(&self.state))));
        // Registered before the check below, not after: a concurrent `cancel` on an ancestor then
        // either walks a list that already contains this child or has already set the flag this
        // check reads. The other order leaves a gap where both miss and the child starts live.
        self.state.adopt(&state);
        let scope = Self { state };
        if scope.state.any_cancelled() {
            scope.state.cancel_tree(CancelReason::Parent);
        }
        scope
    }

    /// Requests cancellation, which propagates to every descendant.
    ///
    /// The reason is [`CancelReason::UserRequest`]: a caller with a more precise cause — a deadline,
    /// a failure — calls [`Scope::cancel_with`] instead. Cancelling twice changes nothing and is not
    /// an error: the first cause and the first cleanup run are the ones that count.
    pub fn cancel(&self) {
        self.state.cancel_tree(CancelReason::UserRequest);
    }

    /// Requests cancellation with a specific cause, which propagates to every descendant.
    ///
    /// Only the first cause is kept. A later call still marks this node and any descendant it has
    /// not reached, but it does not overwrite the reason already recorded: the work was already
    /// stopping, and rewriting the cause would misattribute the stop.
    pub fn cancel_with(&self, reason: CancelReason) {
        self.state.cancel_tree(reason);
    }

    /// Whether this scope or any ancestor is cancelled.
    ///
    /// Ancestors are checked, not just this node, so the answer stays correct for a scope that was
    /// created in the moment its ancestor was cancelled: propagation marks that child too, and this
    /// walk is what makes the answer independent of the order the two happened in.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.any_cancelled()
    }

    /// Why this scope is cancelled, or `None` while it is not.
    ///
    /// A scope cancelled through an ancestor reports [`CancelReason::Parent`], never the ancestor's
    /// own reason: the distinction is what identifies the scope that was actually asked to stop.
    #[must_use]
    pub fn reason(&self) -> Option<CancelReason> {
        if let Some(reason) = self.state.lock_reason().clone() {
            return Some(reason);
        }
        // Defensive: propagation marks every descendant, so a cancelled ancestor whose reason was
        // missed here should be impossible. Reporting `Parent` rather than `None` means a bug in
        // propagation degrades into a coarser answer instead of a cancelled scope with no cause.
        if self.state.any_cancelled() {
            return Some(CancelReason::Parent);
        }
        None
    }

    /// Registers an action to run when this scope is cancelled.
    ///
    /// The action runs at most once, from [`run_cleanups`], and only if the scope is cancelled. An
    /// action registered on an already-cancelled scope runs immediately instead of being queued:
    /// dropping it would leak exactly what the caller is trying to release, and a cleanup that
    /// re-registers itself therefore recurses — a caller that needs to loop should not use this.
    pub fn on_cancel(&self, action: Box<dyn FnOnce() + Send>) {
        // The check and the push happen under the same lock `run_cleanups` drains under, so an
        // action cannot be pushed into a list that has already been taken.
        let mut cleanups = self.state.lock_cleanups();
        if self.state.any_cancelled() {
            drop(cleanups);
            action();
            return;
        }
        cleanups.actions.push(action);
    }

    /// Blocks until this scope is cancelled.
    ///
    /// Used by a task that has nothing else to do but wait for its scope to end — a background job
    /// holding a handle, a bridge between the tree and a `tokio` task. Waiting on this scope rather
    /// than on an ancestor is enough because propagation sets the flag here too.
    pub fn wait_until_cancelled(&self) {
        let mut guard = self.state.lock_reason();
        while !self.state.cancelled.load(Ordering::SeqCst) {
            // A poisoned mutex means a waiting thread panicked; the flag is still the truth, so
            // keep waiting on the inner guard rather than propagating a panic out of a wait.
            guard = self
                .state
                .signal
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Runs the cleanups registered on `scope` and on every cancelled descendant, each exactly once.
///
/// Called by the owner after cancellation, rather than from [`Drop`], so the moment teardown runs
/// is the caller's choice. Safe to call more than once and safe to call on a scope that is not
/// cancelled, in which case nothing runs — a shutdown path can call it unconditionally.
///
/// Descendants are included because a cancelled parent cancels its children, so their cleanups are
/// part of the same teardown; requiring the caller to visit every node would mean keeping handles
/// it has no other use for. A descendant that was cancelled on its own also runs here, which is the
/// common shape: a subagent stops, its session continues.
pub fn run_cleanups(scope: &Scope) {
    scope.state.run_cleanups_tree();
}
