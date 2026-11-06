//! Plugin lifecycle: dependency-gated activation and orderly teardown.
//!
//! Upstream Cordis gives every plugin application a *fiber*. A fiber waits until the services it
//! injects are published, runs the plugin callback when they are, unloads when one of them goes
//! away, and unloads for good when its owner unloads. This module is that state machine.
//!
//! # What changed from upstream
//!
//! Upstream waits for dependents to drain with a spin loop:
//!
//! ```text
//! while ([...this.provisions].some(impl => impl.dependents.size)) {
//!   await new Promise(resolve => this._release = resolve)
//! }
//! ```
//!
//! Each iteration re-scans every provision and parks on a promise that is resolved by a *later*
//! unload, so N dependents cost N wake-ups and N full scans. Under tokio the parked task also
//! holds an executor slot. Here the wait parks on a [`Notify`] that the last dependent to release
//! wakes directly. The loop remains, because two dependents can release concurrently and only one
//! sees the set become empty, but the common path never re-scans.
//!
//! Upstream tracks dependency identity by formatting a string — `revision + ':' + uid` per injected
//! service — and compares the result to detect change. This module bumps a counter instead, which
//! carries the same meaning without an allocation per check.
//!
//! # What is deliberately not here
//!
//! Config resolution and the `internal/config` waterfall belong to phase 3, so a fiber runs a body
//! and does not own a validated config. The event bus belongs to phase 2, so state transitions are
//! observable through [`Fiber::subscribe`] rather than emitted as an `internal/status` event.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::scope::BoxFut;

/// Lifecycle state of one plugin application.
///
/// Transitions: `Pending -> Loading -> Active`, `Active -> Unloading -> Pending` when a dependency
/// goes away, and `* -> Disposed` once the owner unloads. `Failed` ends the current activation, but
/// a later dependency change starts a fresh one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FiberState {
    /// Waiting for one or more injected services to be published.
    Pending,
    /// The plugin body is running.
    Loading,
    /// The plugin body finished; the fiber is providing.
    Active,
    /// The plugin body returned an error.
    ///
    /// Unlike [`FiberState::Active`], this is reached only when the body reports a failure before
    /// producing cleanup. A later [`Fiber::start`] may retry the body.
    Failed,
    /// Disposers are running.
    Unloading,
    /// The fiber was removed and cannot restart.
    Disposed,
}

impl FiberState {
    /// Returns true when the plugin body is running or has finished successfully.
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Loading | Self::Active)
    }

    /// Returns true when the fiber has been torn down for good.
    #[must_use]
    pub fn is_disposed(self) -> bool {
        matches!(self, Self::Disposed)
    }
}

/// A failure raised while driving a fiber.
#[derive(Debug, thiserror::Error)]
pub enum FiberError {
    /// The plugin body returned an error.
    #[error("plugin `{plugin}` failed: {source}")]
    Plugin {
        /// Display name of the plugin.
        plugin: String,
        /// The error it returned.
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// An operation required a live fiber but it had already been disposed.
    #[error("plugin `{plugin}` is disposed")]
    Disposed {
        /// Display name of the plugin.
        plugin: String,
    },
}

/// One cleanup action contributed by a plugin body.
///
/// `FnOnce` rather than `Fn` for the same reason [`Scope`](crate::Scope) uses it: running a
/// disposer twice is a type error, not a runtime flag to check.
pub type Disposer = Box<dyn FnOnce() -> BoxFut<()> + Send>;

/// The plugin body, run once per activation.
///
/// It is `Fn` rather than `FnOnce` because a fiber re-runs it after every dependency change, which
/// is what upstream does when it re-executes the plugin callback on reload. The body yields the
/// disposer for the work it did, so running it twice must produce independent resources; a body
/// that cannot do that should return a disposer that fails loudly rather than silently skip
/// cleanup.
pub type PluginBody = Box<dyn Fn() -> BoxFut<Result<Disposer, BodyError>> + Send + Sync>;

/// The error a plugin body may report.
///
/// A boxed error rather than a generic parameter: the fiber stores plugins of different concrete
/// types behind one `PluginBody`, so the failure type has to be erased at the same boundary.
pub type BodyError = Box<dyn std::error::Error + Send + Sync>;

/// Declarative description of what a plugin contributes and needs.
pub struct PluginSpec {
    /// Display name, used in errors and in [`Fiber::name`].
    pub name: String,
    /// Service ids whose publication gates this plugin.
    pub inject: Vec<&'static str>,
    /// The plugin body.
    pub run: PluginBody,
}

impl std::fmt::Debug for PluginSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginSpec")
            .field("name", &self.name)
            .field("inject", &self.inject)
            .finish_non_exhaustive()
    }
}

/// Monotonic stamp distinguishing one activation attempt from the next.
///
/// Upstream composes a string per injected service and compares it; a counter carries the same
/// information, because the only question is whether the dependency set changed since an attempt
/// began.
static REVISION: AtomicU64 = AtomicU64::new(1);

fn next_revision() -> u64 {
    REVISION.fetch_add(1, Ordering::Relaxed)
}

/// Everything mutable about a fiber, behind one lock.
struct Inner {
    state: FiberState,
    /// Bumped whenever the dependency set changes, so an in-flight load can tell it was superseded.
    revision: u64,
    /// Injected ids that are currently published.
    satisfied: BTreeSet<&'static str>,
    /// uids of fibers that injected a service this fiber provides and have not released it.
    dependents: BTreeSet<u64>,
    /// Cleanup contributed by the current activation, in registration order.
    disposers: Vec<Disposer>,
    /// Observers of state changes.
    subscribers: Vec<tokio::sync::mpsc::UnboundedSender<FiberState>>,
}

impl Inner {
    fn new() -> Self {
        Self {
            state: FiberState::Pending,
            revision: next_revision(),
            satisfied: BTreeSet::new(),
            dependents: BTreeSet::new(),
            disposers: Vec::new(),
            subscribers: Vec::new(),
        }
    }
}

/// One plugin application.
///
/// Cheap to clone through `Arc`; clones share one state machine. Created in [`FiberState::Pending`]
/// and activated by [`Fiber::start`] once its injected services are available.
pub struct Fiber {
    uid: u64,
    spec: Arc<PluginSpec>,
    inner: Arc<Mutex<Inner>>,
    /// Wakes dependency waits and [`Fiber::wait_ready`]. Notifications are sticky, so a wake that
    /// arrives before a waiter parks is still observed.
    signal: Arc<Notify>,
    /// Serialises lifecycle transitions: two concurrent `start` calls must not both run the body,
    /// and an `unload` must not overtake a `start` that is mid-flight.
    lifecycle: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Fiber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fiber")
            .field("uid", &self.uid)
            .field("name", &self.spec.name)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl Fiber {
    /// Creates a pending fiber.
    #[must_use]
    pub fn new(uid: u64, spec: PluginSpec) -> Arc<Self> {
        Arc::new(Self {
            uid,
            spec: Arc::new(spec),
            inner: Arc::new(Mutex::new(Inner::new())),
            signal: Arc::new(Notify::new()),
            lifecycle: tokio::sync::Mutex::new(()),
        })
    }

    /// The fiber's identity within its owner.
    #[must_use]
    pub fn uid(&self) -> u64 {
        self.uid
    }

    /// The plugin's display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.spec.name
    }

    /// The service ids this plugin declared it needs.
    #[must_use]
    pub fn injects(&self) -> &[&'static str] {
        &self.spec.inject
    }

    /// The current lifecycle state.
    #[must_use]
    pub fn state(&self) -> FiberState {
        self.lock().state
    }

    /// Returns true when the fiber is [`FiberState::Active`].
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state() == FiberState::Active
    }

    /// Number of dependents currently holding this fiber's services.
    #[must_use]
    pub fn dependent_count(&self) -> usize {
        self.lock().dependents.len()
    }

    /// Returns a receiver of every state change from now on, ending when the fiber is disposed.
    ///
    /// Stands in for upstream's `internal/status` event until the event bus exists (phase 2). The
    /// channel is unbounded so a transition — which happens under the lifecycle lock — never blocks
    /// on a slow observer.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<FiberState> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.lock().subscribers.push(tx);
        rx
    }

    /// Reports which services are currently published, returning the fiber to `Pending` when an
    /// injected one disappears.
    ///
    /// The owner calls this whenever the service table changes. Ids the fiber does not inject are
    /// ignored, so the caller can pass the whole table without filtering.
    ///
    /// Losing a dependency tears the plugin down before this returns: the plugin must not keep
    /// running against a service that no longer exists, and its disposers must still run. That is
    /// why this is `async` where upstream's equivalent check is synchronous.
    pub async fn observe_services(&self, available: impl IntoIterator<Item = &'static str>) {
        let available: BTreeSet<&'static str> = available.into_iter().collect();
        let lost_dependency = {
            let mut inner = self.lock();
            let next: BTreeSet<&'static str> = self
                .spec
                .inject
                .iter()
                .copied()
                .filter(|id| available.contains(id))
                .collect();
            if next == inner.satisfied {
                return;
            }
            let still_complete = next.len() == self.spec.inject.len();
            inner.satisfied = next;
            inner.revision = next_revision();
            !still_complete && inner.state.is_active()
        };

        if lost_dependency {
            // Losing a dependency takes an active fiber down immediately: the plugin must not keep
            // running against a service that no longer exists. Teardown is *not* skipped — it is
            // driven through the same path as an ordinary unload so the body's disposer runs and
            // the fiber ends in `Pending` with nothing outstanding.
            self.unload_inner().await;
        }
    }

    /// Returns true when every injected service is published.
    #[must_use]
    pub fn dependencies_met(&self) -> bool {
        self.lock().satisfied.len() == self.spec.inject.len()
    }

    /// Runs the plugin body if its dependencies are met.
    ///
    /// Runs the body at most once per activation even when callers race: the lifecycle lock
    /// serialises them, and a caller that arrives second observes `Active` and returns.
    ///
    /// # Errors
    ///
    /// Returns [`FiberError::Plugin`] when the body fails and [`FiberError::Disposed`] when the
    /// fiber has already been torn down.
    pub async fn start(self: &Arc<Self>) -> Result<(), FiberError> {
        let _guard = self.lifecycle.lock().await;

        match self.state() {
            FiberState::Disposed => {
                return Err(FiberError::Disposed {
                    plugin: self.spec.name.clone(),
                });
            }
            FiberState::Active | FiberState::Loading => return Ok(()),
            _ => {}
        }

        if !self.dependencies_met() {
            // Still missing something. The owner calls `start` again after a table change; parking
            // here would make the caller responsible for a wake-up it does not control.
            return Ok(());
        }

        let revision = {
            let mut inner = self.lock();
            inner.state = FiberState::Loading;
            inner.revision
        };
        self.notify(FiberState::Loading);

        let ran = (self.spec.run)().await;

        let dispose = match ran {
            Ok(dispose) => dispose,
            Err(source) => {
                // The body failed before it produced any cleanup. Record `Failed` so the state is
                // honest — a caller looking at `state()` must not see `Active` for a plugin that
                // never started — and surface the error to whoever awaited `start`.
                let mut inner = self.lock();
                inner.state = FiberState::Failed;
                inner.revision = next_revision();
                drop(inner);
                self.notify(FiberState::Failed);
                self.signal.notify_waiters();
                return Err(FiberError::Plugin {
                    plugin: self.spec.name.clone(),
                    source,
                });
            }
        };

        // Decide under the lock, act outside it: the guard is not `Send` and must not be alive
        // across the `await` below.
        let (stale, disposed) = {
            let inner = self.lock();
            let stale = inner.revision != revision || inner.state.is_disposed();
            (stale, inner.state.is_disposed())
        };

        if stale {
            // A dependency changed while the body ran, so this activation is already superseded.
            // Its resources still have to be released, but it never reports Active.
            dispose().await;
            return if disposed {
                Err(FiberError::Disposed {
                    plugin: self.spec.name.clone(),
                })
            } else {
                Ok(())
            };
        }

        {
            let mut inner = self.lock();
            inner.disposers.push(dispose);
            inner.state = FiberState::Active;
        }

        self.notify(FiberState::Active);
        self.signal.notify_waiters();
        Ok(())
    }

    /// Runs the current activation's disposers in reverse order and returns to `Pending`.
    ///
    /// Dependents drain first: one may still resolve this fiber's services during its own
    /// teardown, and releasing early would hand it a dead service. That ordering is the whole
    /// reason the dependent set exists.
    pub async fn unload(self: &Arc<Self>) {
        self.unload_inner().await;
    }

    /// The shared body of [`Fiber::unload`].
    ///
    /// Split out because [`Fiber::observe_services`] takes `&self` and still has to drive teardown.
    /// Everything here works through `Arc::clone(self)`-free access: it only needs the inner state
    /// and the signal, both already `Arc`-shared.
    async fn unload_inner(&self) {
        let _guard = self.lifecycle.lock().await;

        {
            let mut inner = self.lock();
            if !inner.state.is_active() {
                return;
            }
            inner.state = FiberState::Unloading;
        }
        self.notify(FiberState::Unloading);

        self.wait_for_dependents_gone().await;

        let pending = {
            let mut inner = self.lock();
            let disposers = std::mem::take(&mut inner.disposers);
            inner.state = FiberState::Pending;
            inner.revision = next_revision();
            disposers
        };

        for dispose in pending.into_iter().rev() {
            dispose().await;
        }

        self.notify(FiberState::Pending);
        self.signal.notify_waiters();
    }

    /// Unloads for good.
    ///
    /// Afterwards [`Fiber::start`] reports [`FiberError::Disposed`] and the subscription channel
    /// closes. Calling it twice is a no-op.
    pub async fn dispose(self: &Arc<Self>) {
        let _guard = self.lifecycle.lock().await;

        if self.state().is_disposed() {
            return;
        }
        {
            let mut inner = self.lock();
            inner.state = FiberState::Unloading;
        }
        self.notify(FiberState::Unloading);

        self.wait_for_dependents_gone().await;

        let (pending, subscribers) = {
            let mut inner = self.lock();
            let disposers = std::mem::take(&mut inner.disposers);
            let subscribers = std::mem::take(&mut inner.subscribers);
            inner.state = FiberState::Disposed;
            (disposers, subscribers)
        };

        for dispose in pending.into_iter().rev() {
            dispose().await;
        }

        // Dropping the senders ends every observer's `recv` loop.
        drop(subscribers);
        self.signal.notify_waiters();
        self.notify(FiberState::Disposed);
    }

    /// Records that `dependent` now holds this fiber's services.
    pub fn add_dependent(&self, dependent: u64) {
        self.lock().dependents.insert(dependent);
    }

    /// Records that `dependent` no longer holds this fiber's services.
    ///
    /// Wakes the teardown waiter when this was the last dependent — the notification upstream's
    /// spin loop waits for, delivered directly instead of polled.
    pub fn release_dependent(&self, dependent: u64) {
        let last = {
            let mut inner = self.lock();
            if !inner.dependents.remove(&dependent) {
                return;
            }
            inner.dependents.is_empty()
        };
        if last {
            self.signal.notify_waiters();
        }
    }

    /// Waits until no dependent holds this fiber's services.
    ///
    /// Replaces upstream's spin loop. It is still a loop, because two dependents can release
    /// concurrently and only one observes the set become empty, but each pass parks on a
    /// notification rather than on a promise that may already be resolved. Interest is registered
    /// *before* the check so a release between them cannot be missed.
    async fn wait_for_dependents_gone(&self) {
        loop {
            let notified = self.signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.lock().dependents.is_empty() {
                return;
            }
            notified.await;
        }
    }

    /// Waits until the fiber leaves `Pending`, returning the state it settled in.
    pub async fn wait_ready(&self) -> FiberState {
        loop {
            let notified = self.signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let state = self.state();
            if state != FiberState::Pending {
                return state;
            }
            notified.await;
        }
    }

    /// Spawns the plugin body, returning a handle that settles when activation finishes.
    #[must_use]
    pub fn spawn_start(self: &Arc<Self>) -> JoinHandle<Result<(), FiberError>> {
        let fiber = Arc::clone(self);
        tokio::spawn(async move { fiber.start().await })
    }

    fn notify(&self, state: FiberState) {
        let mut inner = self.lock();
        inner.subscribers.retain(|tx| tx.send(state).is_ok());
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
