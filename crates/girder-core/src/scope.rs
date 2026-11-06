//! Effect scopes and reverse-order disposal.
//!
//! Upstream Cordis records every contribution a plugin makes (a service, an event listener, a
//! timer) as an *effect*: the plugin returns a disposer, and the owning fiber runs those disposers
//! in reverse registration order when it unloads. Nothing relies on the plugin remembering to
//! clean up.
//!
//! The upstream disposer list is guarded by a `disposing` flag, because a disposer can be called
//! twice and the second call must be a no-op. Rust does not need the flag: disposers are `FnOnce`,
//! so running one twice is a compile error rather than a runtime check. What Rust does need — and
//! Node did not — is help with *when* to run them, since a `Drop` implementation cannot await.
//! [`Scope::dispose`] is therefore an explicit `async fn`, called by whoever owns the scope.
//! Parking cleanup in `Drop` is deferred to the fiber work in phase 1, where the registry knows
//! enough to distinguish "disposed on purpose" from "leaked".

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

/// A boxed future that is `Send` and owns everything it touches.
///
/// The single type parameter is deliberate. An earlier sketch wrote
/// `dyn for<'a> Fn(T) -> BoxFut<'a, T>`, which fails to compile with E0582 (a lifetime bound on
/// the higher-ranked binder appears in the return type). Keeping `BoxFut` lifetime-free forces
/// arguments and results to be owned, which is the constraint the plan records in §1.3.
pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Cleanup registered by an effect, run once when the scope is disposed.
pub type BoxDisposer = Box<dyn FnOnce() -> BoxFut<()> + Send + 'static>;

/// A named batch of disposers that unwinds in reverse registration order.
///
/// ```
/// # use girder_core::Scope;
/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
/// let scope = Scope::new();
/// scope.effect("first", || Box::pin(async { println!("first") }));
/// scope.effect("second", || Box::pin(async { println!("second") }));
/// scope.dispose().await; // prints "second", then "first"
/// # });
/// ```
///
/// Disposal order is the mirror of construction order so that a later effect may depend on an
/// earlier one and still be torn down first — the same guarantee the upstream `DisposableList`
/// provides by reversing its list.
#[derive(Default)]
pub struct Scope {
    /// Effects not yet disposed, in registration order. Drain reversed on disposal.
    ///
    /// A `std::sync::Mutex` is correct here even though disposal is async: the lock is only held
    /// while moving disposers out, never across an `await`. Holding a std mutex across an await
    /// would block the executor thread, and a disposer that re-entered this scope would deadlock.
    disposers: Mutex<Vec<(&'static str, BoxDisposer)>>,
}

impl Scope {
    /// Creates an empty scope.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a disposer to run when the scope is disposed.
    ///
    /// `label` identifies the effect in diagnostics and in the disposal-order test; it is the
    /// Rust equivalent of the upstream effect description (for example `ctx.on("event")` or
    /// `ctx.provide("name")`).
    pub fn effect<F>(&self, label: &'static str, dispose: F)
    where
        F: FnOnce() -> BoxFut<()> + Send + 'static,
    {
        self.lock().push((label, Box::new(dispose)));
    }

    /// Number of effects that have not been disposed yet.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Returns true when no effect is outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Runs every outstanding disposer in reverse registration order.
    ///
    /// Disposers are moved out of the lock before any of them runs. A disposer that panics
    /// therefore unwinds the caller, but the scope is left empty rather than half-poisoned, and
    /// the remaining disposers are *not* run — the plan treats a panicking teardown as a bug to
    /// surface, not a condition to swallow. Calling `dispose` twice is safe: the second call finds
    /// nothing to do.
    pub async fn dispose(&self) {
        let pending = std::mem::take(&mut *self.lock());

        for (_, dispose) in pending.into_iter().rev() {
            dispose().await;
        }
    }

    /// Locks the disposer list, recovering from a poisoned mutex.
    ///
    /// Poisoning means a disposer panicked. The list holds owned boxed closures, so it stays
    /// structurally valid; the panic has already been reported to the caller that ran it.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(&'static str, BoxDisposer)>> {
        self.disposers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let labels: Vec<&'static str> = self.lock().iter().map(|(label, _)| *label).collect();
        f.debug_struct("Scope").field("pending", &labels).finish()
    }
}
