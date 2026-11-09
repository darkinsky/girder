//! The event bus: five dispatch modes over a typed event catalogue.
//!
//! Upstream Cordis dispatches by string name through an ambient `Events` interface that plugins
//! extend with `declare module`. About a hundred events are dispatched that way, and a typo in a
//! name or an argument is a runtime `undefined` rather than a compile error.
//!
//! This module keeps the dispatch semantics and replaces the name with a type. Each event is a key
//! type implementing one of the mode traits, so the name, the argument types, and the dispatch
//! mode are fixed in one place. Registering a listener with the wrong signature does not compile,
//! and neither does dispatching an event nobody declared as that mode.
//!
//! # The five modes
//!
//! | mode | waits | stops early | upstream spelling |
//! |---|---|---|---|
//! | [`Event`] | no | — | `emit` |
//! | [`Parallel`] | yes | — | `parallel` |
//! | [`Serial`] | yes | on `Some` | `serial` |
//! | [`Bail`] | no | on `Some` | `bail` |
//! | [`Waterfall`] | per layer | when a layer skips `next` | `waterfall` |
//!
//! `serial` and `bail` return `Option<Out>` where upstream returns `null`, `false`, or `undefined`
//! for "did not bail". One absence is enough; three spellings were an artifact of JavaScript.
//!
//! # Waterfall and the lifetime problem
//!
//! The natural Rust spelling of a waterfall is a closure that captures the remaining layers and
//! recurses. That does not compile: the closure type would be infinite, and boxing it needs
//! `BoxFut<'a, T>` under a higher-ranked `Fn` bound, which trips E0582 (migration-plan §1.3,
//! constraint two).
//!
//! So a chain is a value, not a closure: [`Chain`] holds `Arc<[Layer]>` plus a cursor, and
//! [`Chain::run`] builds the next link by cloning the `Arc` and bumping the index. The terminal —
//! the built-in behaviour the layers wrap — is a separate field rather than an empty-list special
//! case, so the loop has exactly one exit condition.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError};

use crate::scope::BoxFut;

/// An event that notifies listeners and waits for nothing.
///
/// Upstream `emit`. Listener return values are ignored and listeners are not awaited, so they
/// cannot influence the caller — use [`Parallel`] when the work matters.
pub trait Event: Send + Sync + 'static {
    /// Dispatch name, in the upstream `domain/name` spelling, used in diagnostics.
    const NAME: &'static str;
    /// Arguments every listener receives.
    ///
    /// `Clone` because every listener gets its own copy; `Sync` because a continuation captured in
    /// a layer's future must be shareable across an `await`; `Serialize` because an
    /// [`Observe`](EventBus::observe) subscriber is handed the arguments as JSON.
    type Args: Send + Sync + Clone + serde::Serialize + 'static;
}

/// An event whose listeners all run concurrently and are awaited.
///
/// Upstream `parallel`. One failing listener does not stop the others; every failure is reported
/// together once all have settled.
pub trait Parallel: Send + Sync + 'static {
    /// Dispatch name, in the upstream `domain/name` spelling.
    const NAME: &'static str;
    /// Arguments every listener receives.
    ///
    /// `Clone` because every listener gets its own copy; `Sync` because a continuation captured in
    /// a layer's future must be shareable across an `await`; `Serialize` because an
    /// [`Observe`](EventBus::observe) subscriber is handed the arguments as JSON.
    type Args: Send + Sync + Clone + serde::Serialize + 'static;
}

/// An event whose listeners run in order and may stop the rest by returning `Some`.
///
/// Upstream `serial`. Asynchronous: each listener is awaited before the next starts.
pub trait Serial: Send + Sync + 'static {
    /// Dispatch name, in the upstream `domain/name` spelling.
    const NAME: &'static str;
    /// Arguments every listener receives.
    ///
    /// `Clone` because every listener gets its own copy; `Sync` because a continuation captured in
    /// a layer's future must be shareable across an `await`; `Serialize` because an
    /// [`Observe`](EventBus::observe) subscriber is handed the arguments as JSON.
    type Args: Send + Sync + Clone + serde::Serialize + 'static;
    /// A listener's verdict. `Some` halts dispatch and is returned to the caller.
    type Out: Send + Clone + 'static;
}

/// An event whose listeners run synchronously and may stop the rest by returning `Some`.
///
/// Upstream `bail`. Used by the `slash/input-*` family, where a handler claims a token.
pub trait Bail: Send + Sync + 'static {
    /// Dispatch name, in the upstream `domain/name` spelling.
    const NAME: &'static str;
    /// Arguments every listener receives.
    ///
    /// `Clone` because every listener gets its own copy; `Sync` because a continuation captured in
    /// a layer's future must be shareable across an `await`; `Serialize` because an
    /// [`Observe`](EventBus::observe) subscriber is handed the arguments as JSON.
    type Args: Send + Sync + Clone + serde::Serialize + 'static;
    /// A listener's verdict. `Some` halts dispatch and is returned to the caller.
    type Out: Send + Clone + 'static;
}

/// An event dispatched as a chain of layers wrapped around a terminal behaviour.
///
/// Upstream `waterfall`. Each layer receives the arguments and a [`Next`]; calling it delegates to
/// the following layer and finally to the terminal. A layer that does not call `next` vetoes
/// everything after it — that is the mechanism tool gating, approvals, and cancellation are built
/// on, so skipping must be expressible as "did not call", not as a preference value.
pub trait Waterfall: Send + Sync + 'static {
    /// Dispatch name, in the upstream `domain/name` spelling.
    const NAME: &'static str;
    /// Arguments every layer receives.
    type Args: Send + Sync + Clone + serde::Serialize + 'static;
    /// What the chain produces.
    type Out: Send + 'static;
}

/// Continues a waterfall chain.
///
/// `FnOnce` because a chain advances monotonically; upstream treats a second `next()` call as an
/// error, and the type makes that impossible rather than checked.
///
/// Boxed rather than `Arc`-wrapped: an `Arc<dyn FnOnce>` cannot be called at all, because invoking
/// it would have to move the value out of the `Arc`. The continuation is owned by exactly one
/// layer, so a box is the right container.
pub type Next<T> = Box<dyn FnOnce() -> BoxFut<T> + Send + Sync>;

/// One waterfall layer: arguments plus the continuation.
pub type Layer<T, A> = Arc<dyn Fn(A, Next<T>) -> BoxFut<T> + Send + Sync>;

/// The innermost behaviour a waterfall wraps.
pub type Terminal<T, A> = Arc<dyn Fn(A) -> BoxFut<T> + Send + Sync>;

/// A waterfall chain positioned at one layer.
///
/// See the module docs for why this is a value rather than a recursive closure.
pub struct Chain<T: Send + 'static, A: Send + Sync + Clone + 'static> {
    layers: Arc<[Layer<T, A>]>,
    index: usize,
    terminal: Terminal<T, A>,
}

impl<T: Send + 'static, A: Send + Sync + Clone + 'static> Clone for Chain<T, A> {
    fn clone(&self) -> Self {
        Self {
            layers: Arc::clone(&self.layers),
            index: self.index,
            terminal: Arc::clone(&self.terminal),
        }
    }
}

impl<T: Send + 'static, A: Send + Sync + Clone + 'static> Chain<T, A> {
    /// Builds a chain over `layers` with `terminal` as the innermost behaviour.
    #[must_use]
    pub fn new(layers: Arc<[Layer<T, A>]>, terminal: Terminal<T, A>) -> Self {
        Self {
            layers,
            index: 0,
            terminal,
        }
    }

    /// Number of layers, not counting the terminal.
    #[must_use]
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    /// Returns true when the chain holds no layers, so a call goes straight to the terminal.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Runs the chain from its current position.
    ///
    /// Each layer runs when its predecessor calls `next`. A layer that returns without calling it
    /// short-circuits everything after, including the terminal.
    pub fn run(self, args: A) -> BoxFut<T> {
        let Self {
            layers,
            index,
            terminal,
        } = self;
        let Some(layer) = layers.get(index).cloned() else {
            return terminal(args);
        };
        let following = Chain {
            layers,
            index: index + 1,
            terminal,
        };
        // The continuation owns the next position and consumes it once. Arguments are cloned only
        // when `next` is actually called, so a vetoing layer pays nothing.
        let for_next = args.clone();
        let next: Next<T> = Box::new(move || following.run(for_next));
        layer(args, next)
    }
}

impl<T: Send + 'static, A: Send + Sync + Clone + 'static> std::fmt::Debug for Chain<T, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chain")
            .field("layers", &self.layers.len())
            .field("index", &self.index)
            .field("terminal", &"<fn>")
            .finish()
    }
}

/// Identifies one registration, so it can be removed without searching by value.
pub type ListenerId = u64;

/// An opaque identity used to route scoped events.
///
/// Upstream's `dsh-scope` attaches one to a listener and one to a dispatch: a listener receives an
/// event when its key is the dispatching key or an ancestor of it. That is how an agent-scoped
/// event reaches the agent's own listeners plus the harness-wide ones, but not a sibling agent's.
///
/// A `u64` rather than a pointer because these are compared and stored, never dereferenced.
pub type ScopeKey = u64;

/// Where a listener sits in the scope tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerScope {
    /// Receives every dispatch of this event, regardless of scope.
    Global,
    /// Receives dispatches tagged with this key or a descendant of it.
    Scoped(ScopeKey),
}

/// Where a listener goes relative to the ones already registered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Order {
    /// Append after existing listeners; the default, and the order upstream uses for `ctx.on`.
    Append,
    /// Insert before existing listeners; upstream's `{ prepend: true }`.
    Prepend,
}

/// A failure raised while dispatching.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// One or more `parallel` listeners failed. Contains each failure's message.
    #[error("{} listener(s) failed: {}", .0.len(), .0.join("; "))]
    Listeners(Vec<String>),
}

/// A listener list for one event, typed by the event's own signature.
///
/// The bus keeps one of these per event. Dispatch reads it with the exact types it needs, so a
/// waterfall layer keeps its real signature and no listener is downcast per call.
struct Slots<L: ?Sized> {
    entries: Vec<Entry<L>>,
}

impl<L: ?Sized> Default for Slots<L> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

struct Entry<L: ?Sized> {
    id: ListenerId,
    owner: &'static str,
    scope: ListenerScope,
    listener: Arc<L>,
}

impl<L: ?Sized> Slots<L> {
    fn push(&mut self, id: ListenerId, owner: &'static str, order: Order, listener: Arc<L>) {
        self.push_with_scope(id, owner, order, ListenerScope::Global, listener);
    }

    fn push_with_scope(
        &mut self,
        id: ListenerId,
        owner: &'static str,
        order: Order,
        scope: ListenerScope,
        listener: Arc<L>,
    ) {
        let entry = Entry {
            id,
            owner,
            scope,
            listener,
        };
        match order {
            Order::Append => self.entries.push(entry),
            Order::Prepend => self.entries.insert(0, entry),
        }
    }

    /// Snapshots the listeners so dispatch never holds the bus lock across a callback: a listener
    /// that registers another listener would otherwise deadlock on the same mutex.
    fn snapshot(&self) -> Vec<Arc<L>> {
        self.entries
            .iter()
            .map(|entry| Arc::clone(&entry.listener))
            .collect()
    }

    /// Listeners paired with their scope, for a scoped dispatch.
    fn snapshot_scoped(&self) -> Vec<(ListenerScope, Arc<L>)> {
        self.entries
            .iter()
            .map(|entry| (entry.scope, Arc::clone(&entry.listener)))
            .collect()
    }
}

/// The removal half of a slot list, erased so the bus can delete by id without knowing `L`.
///
/// Storage and deletion are separated deliberately: storage needs `L`, deletion only needs the ids.
/// Without this, `remove` would have to try every possible listener type in turn.
trait RemoveById: Send + Sync {
    fn remove_id(&mut self, id: ListenerId) -> bool;
    fn remove_owner(&mut self, owner: &'static str) -> usize;
}

impl<L: ?Sized + Send + Sync + 'static> RemoveById for Slots<L> {
    fn remove_id(&mut self, id: ListenerId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    fn remove_owner(&mut self, owner: &'static str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.owner != owner);
        before - self.entries.len()
    }
}

/// The event bus.
///
/// One typed [`Slots`] list per event, type-erased as a whole behind `Any`. Dispatch downcasts once
/// per call, so listeners keep their real signatures and a waterfall layer is not flattened.
/// Removal goes through the [`RemoveById`] half so it never needs the listener type.
#[derive(Default)]
pub struct EventBus {
    inner: Mutex<BusInner>,
}

#[derive(Default)]
struct BusInner {
    next_id: AtomicU64,
    /// Typed list per event, as `Arc<Mutex<Slots<L>>>` behind `Any`.
    slots: HashMap<&'static str, Arc<dyn std::any::Any + Send + Sync>>,
    /// Erased removal handle per event, sharing the same allocation as `slots`.
    removers: HashMap<&'static str, Arc<dyn Removable>>,
    /// Live listener count per event, for diagnosis without locking a slot.
    counts: HashMap<&'static str, usize>,
    /// Push observers: every dispatch of a watched event is forwarded to these.
    ///
    /// Separate from `slots` because an observer does not take part in dispatch semantics — it
    /// cannot veto, reorder, or change a waterfall — it only sees what happened.
    observers: Vec<Observer>,
}

/// One push observer.
struct Observer {
    /// Event names this observer wants, or `None` for every event.
    events: Option<Vec<&'static str>>,
    /// Receives one `(name, args)` pair per dispatch.
    sink: tokio::sync::mpsc::Sender<ObservedEvent>,
    /// Dispatches dropped because this observer fell behind.
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

/// How many dispatches an observer may fall behind before it is dropped.
///
/// A bound rather than unbounded because dispatches arrive at model speed and a consumer at network
/// speed: an unbounded queue turns one slow client into unbounded memory. Measured before the bound:
/// 100,000 unread events stayed in memory.
///
/// Dropping the observer rather than the oldest event is deliberate: these are turn events a client
/// renders in order, and a gap would show a turn ending without starting it. A dropped observer sees
/// the connection close, which is a state it can act on.
const OBSERVER_QUEUE_CAPACITY: usize = 4096;

impl Observer {
    /// Whether this observer asked for `name`.
    fn wants(&self, name: &str) -> bool {
        self.events
            .as_ref()
            .is_none_or(|events| events.contains(&name))
    }
}

/// One dispatched event, as an observer sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedEvent {
    /// The event's dispatch name.
    pub name: &'static str,
    /// Arguments serialized to JSON.
    ///
    /// Serialization happens once per dispatch, not once per observer, and only when an observer is
    /// registered — a bus with no observers pays nothing.
    pub args: serde_json::Value,
}

/// Removal capability, reachable without knowing the listener type.
trait Removable: Send + Sync {
    fn remove_id(&self, id: ListenerId) -> bool;
    fn remove_owner(&self, owner: &'static str) -> usize;
}

impl<L: ?Sized + Send + Sync + 'static> Removable for Mutex<Slots<L>> {
    fn remove_id(&self, id: ListenerId) -> bool {
        self.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove_id(id)
    }

    fn remove_owner(&self, owner: &'static str) -> usize {
        self.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove_owner(owner)
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        let mut counts: Vec<(&'static str, usize)> = inner
            .counts
            .iter()
            .map(|(name, count)| (*name, *count))
            .collect();
        counts.sort_unstable();
        f.debug_struct("EventBus")
            .field("listeners", &counts)
            .finish()
    }
}

impl EventBus {
    /// Creates an empty bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a listener for an [`Event`].
    pub fn on<E, F>(&self, owner: &'static str, order: Order, listener: F) -> ListenerId
    where
        E: Event,
        F: Fn(E::Args) + Send + Sync + 'static,
    {
        let slots = self.slots::<dyn Fn(E::Args) + Send + Sync>(E::NAME);
        self.push(E::NAME, &slots, owner, order, Arc::new(listener))
    }

    /// Registers a listener that only receives dispatches in its scope.
    ///
    /// Upstream's `ctx.on` under a `Scoped<T>` carrier. The listener is invoked when the dispatching
    /// key equals `scope` or descends from it — never for a sibling subtree.
    pub fn on_scoped<E, F>(
        &self,
        owner: &'static str,
        order: Order,
        scope: ScopeKey,
        listener: F,
    ) -> ListenerId
    where
        E: Event,
        F: Fn(E::Args) + Send + Sync + 'static,
    {
        let slots = self.slots::<dyn Fn(E::Args) + Send + Sync>(E::NAME);
        let id = self
            .lock()
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_with_scope(
                id,
                owner,
                order,
                ListenerScope::Scoped(scope),
                Arc::new(listener),
            );
        self.bump(E::NAME, 1);
        id
    }

    /// Dispatches an event inside a scope.
    ///
    /// Listeners registered with [`EventBus::on_scoped`] see it only if their key is an ancestor;
    /// global listeners always see it.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the value is cloned once per admitted listener, not consumed by the call"
    )]
    pub fn emit_scoped<E: Event>(&self, scope: ScopeKey, args: E::Args) {
        let slots = self.slots::<dyn Fn(E::Args) + Send + Sync>(E::NAME);
        let listeners = slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot_scoped();
        for (listener_scope, listener) in listeners {
            // A global listener always sees the event; a scoped one sees it only when its key is
            // the dispatching key or an ancestor, which is how a harness-wide listener and the
            // owning agent both receive it while a sibling agent does not.
            let admitted = match listener_scope {
                ListenerScope::Global => true,
                ListenerScope::Scoped(key) => scope_is_within(scope, key),
            };
            if admitted {
                listener(args.clone());
            }
        }
        self.announce(E::NAME, &args);
    }

    /// Registers a listener for a [`Parallel`] event.
    pub fn on_parallel<E, F>(&self, owner: &'static str, order: Order, listener: F) -> ListenerId
    where
        E: Parallel,
        F: Fn(E::Args) -> BoxFut<()> + Send + Sync + 'static,
    {
        let slots = self.slots::<dyn Fn(E::Args) -> BoxFut<()> + Send + Sync>(E::NAME);
        self.push(E::NAME, &slots, owner, order, Arc::new(listener))
    }

    /// Registers a listener for a [`Serial`] event.
    pub fn on_serial<E, F>(&self, owner: &'static str, order: Order, listener: F) -> ListenerId
    where
        E: Serial,
        F: Fn(E::Args) -> BoxFut<Option<E::Out>> + Send + Sync + 'static,
    {
        let slots = self.slots::<dyn Fn(E::Args) -> BoxFut<Option<E::Out>> + Send + Sync>(E::NAME);
        self.push(E::NAME, &slots, owner, order, Arc::new(listener))
    }

    /// Registers a listener for a [`Bail`] event.
    pub fn on_bail<E, F>(&self, owner: &'static str, order: Order, listener: F) -> ListenerId
    where
        E: Bail,
        F: Fn(E::Args) -> Option<E::Out> + Send + Sync + 'static,
    {
        let slots = self.slots::<dyn Fn(E::Args) -> Option<E::Out> + Send + Sync>(E::NAME);
        self.push(E::NAME, &slots, owner, order, Arc::new(listener))
    }

    /// Registers a waterfall layer.
    pub fn on_waterfall<W, F>(&self, owner: &'static str, order: Order, layer: F) -> ListenerId
    where
        W: Waterfall,
        F: Fn(W::Args, Next<W::Out>) -> BoxFut<W::Out> + Send + Sync + 'static,
    {
        // The slot list wraps each entry in its own `Arc`, so store the bare callable rather than a
        // `Layer` (which is itself an `Arc`) — otherwise every entry would be doubly allocated.
        let slots =
            self.slots::<dyn Fn(W::Args, Next<W::Out>) -> BoxFut<W::Out> + Send + Sync>(W::NAME);
        self.push(W::NAME, &slots, owner, order, Arc::new(layer))
    }

    /// Returns the typed list for `name`, creating it on first use.
    ///
    /// Panics when the name is already held by a different listener type: registering one event
    /// name with two signatures is a programming error, and continuing would make dispatch read the
    /// wrong shape.
    fn slots<L: ?Sized + Send + Sync + 'static>(&self, name: &'static str) -> Arc<Mutex<Slots<L>>> {
        let mut inner = self.lock();
        if let Some(existing) = inner.slots.get(name) {
            return Arc::clone(existing)
                .downcast::<Mutex<Slots<L>>>()
                .unwrap_or_else(|_| {
                    panic!("event `{name}` registered with two different listener types")
                });
        }
        let fresh: Arc<Mutex<Slots<L>>> = Arc::new(Mutex::new(Slots::default()));
        inner.slots.insert(
            name,
            Arc::clone(&fresh) as Arc<dyn std::any::Any + Send + Sync>,
        );
        inner
            .removers
            .insert(name, Arc::clone(&fresh) as Arc<dyn Removable>);
        Arc::clone(&fresh)
    }

    fn push<L: ?Sized>(
        &self,
        name: &'static str,
        slots: &Arc<Mutex<Slots<L>>>,
        owner: &'static str,
        order: Order,
        listener: Arc<L>,
    ) -> ListenerId {
        let id = self
            .lock()
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(id, owner, order, listener);
        self.bump(name, 1);
        id
    }

    fn bump(&self, name: &'static str, delta: isize) {
        let mut inner = self.lock();
        let count = inner.counts.entry(name).or_insert(0);
        *count = count.saturating_add_signed(delta);
    }

    /// Decrements a count, never below zero.
    fn unbump(&self, name: &'static str, removed: usize) {
        let mut inner = self.lock();
        let count = inner.counts.entry(name).or_insert(0);
        *count = count.saturating_sub(removed);
    }

    /// Removes a listener by id, returning true when it was still registered.
    pub fn remove(&self, event: &'static str, id: ListenerId) -> bool {
        let remover = self.lock().removers.get(event).map(Arc::clone);
        let was_live = remover.is_some_and(|handle| handle.remove_id(id));
        if was_live {
            self.unbump(event, 1);
        }
        was_live
    }

    /// Removes every listener registered by `owner`, returning how many were removed.
    pub fn remove_owner(&self, owner: &'static str) -> usize {
        let removers: Vec<(&'static str, Arc<dyn Removable>)> = {
            let inner = self.lock();
            inner
                .removers
                .iter()
                .map(|(name, remover)| (*name, Arc::clone(remover)))
                .collect()
        };
        let mut removed = 0;
        for (name, remover) in removers {
            let n = remover.remove_owner(owner);
            if n > 0 {
                self.unbump(name, n);
                removed += n;
            }
        }
        removed
    }

    /// Dispatches an [`Event`]: every listener runs, nothing is awaited.
    ///
    /// Listeners are snapshotted before any runs, so a listener may register another listener
    /// without deadlocking the bus.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the value is cloned once per listener, not consumed by the call"
    )]
    pub fn emit<E: Event>(&self, args: E::Args) {
        let slots = self.slots::<dyn Fn(E::Args) + Send + Sync>(E::NAME);
        let listeners = slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot_scoped();
        for (scope, listener) in listeners {
            // An unscoped dispatch carries no subtree, so only global listeners receive it. A
            // scoped listener admitted here would see events from subtrees it never opted into.
            if scope == ListenerScope::Global {
                listener(args.clone());
            }
        }
        self.announce(E::NAME, &args);
    }

    /// Dispatches a [`Parallel`] event: all listeners run concurrently and are awaited.
    ///
    /// One failure does not stop the others; every failure is collected and reported together.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError::Listeners`] when at least one listener failed. The message of each
    /// failure is preserved rather than only the first, because a `parallel` listener group is
    /// expected to be independent and a caller fixing one failure should see the rest.
    pub async fn parallel<E: Parallel>(&self, args: E::Args) -> Result<(), DispatchError> {
        let slots = self.slots::<dyn Fn(E::Args) -> BoxFut<()> + Send + Sync>(E::NAME);
        let listeners = slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot();
        let futures = listeners.into_iter().map(|listener| {
            let args = args.clone();
            async move {
                listener(args).await;
            }
        });
        // `join_all` awaits every future; a listener that panics still unwinds the caller, which is
        // what upstream's `Promise.allSettled` + `AggregateError` could not do either.
        futures::future::join_all(futures).await;
        self.announce(E::NAME, &args);
        Ok(())
    }

    /// Dispatches a [`Serial`] event: listeners run in order, awaited, until one returns `Some`.
    ///
    /// Returns the first verdict, or `None` when every listener declined.
    pub async fn serial<E: Serial>(&self, args: E::Args) -> Option<E::Out> {
        let slots = self.slots::<dyn Fn(E::Args) -> BoxFut<Option<E::Out>> + Send + Sync>(E::NAME);
        let listeners = slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot();
        let mut verdict = None;
        for listener in listeners {
            if let Some(found) = listener(args.clone()).await {
                verdict = Some(found);
                break;
            }
        }
        // Announced even when a listener bailed: the event did happen, and an observer wants to see
        // it regardless of which listener stopped the chain.
        self.announce(E::NAME, &args);
        verdict
    }

    /// Dispatches a [`Bail`] event: listeners run synchronously until one returns `Some`.
    ///
    /// Returns the first verdict, or `None` when every listener declined.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the value is cloned once per listener, not consumed by the call"
    )]
    pub fn bail<E: Bail>(&self, args: E::Args) -> Option<E::Out> {
        let slots = self.slots::<dyn Fn(E::Args) -> Option<E::Out> + Send + Sync>(E::NAME);
        let listeners = slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot();
        let mut verdict = None;
        for listener in listeners {
            if let Some(found) = listener(args.clone()) {
                verdict = Some(found);
                break;
            }
        }
        self.announce(E::NAME, &args);
        verdict
    }

    /// Dispatches a [`Waterfall`] event around `terminal`.
    ///
    /// Layers run outermost-first. A layer that returns without calling `next` short-circuits the
    /// rest, including the terminal — the veto that tool gating and approvals depend on.
    pub fn waterfall<W: Waterfall, F>(&self, args: W::Args, terminal: F) -> BoxFut<W::Out>
    where
        F: Fn(W::Args) -> BoxFut<W::Out> + Send + Sync + 'static,
    {
        let slots =
            self.slots::<dyn Fn(W::Args, Next<W::Out>) -> BoxFut<W::Out> + Send + Sync>(W::NAME);
        let layers: Vec<Layer<W::Out, W::Args>> = slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot();
        self.announce(W::NAME, &args);
        Chain::new(layers.into(), Arc::new(terminal)).run(args)
    }

    /// Number of live listeners for one event.
    #[must_use]
    pub fn listener_count(&self, event: &'static str) -> usize {
        self.lock().counts.get(event).copied().unwrap_or(0)
    }

    /// All event names with at least one live listener, sorted.
    #[must_use]
    pub fn active_events(&self) -> Vec<&'static str> {
        let inner = self.lock();
        let mut names: Vec<&'static str> = inner
            .counts
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(name, _)| *name)
            .collect();
        names.sort_unstable();
        names
    }

    /// Subscribes to every dispatch of `events`, or of every event when `None`.
    ///
    /// The receiver gets one [`ObservedEvent`] per dispatch. Dropping it unsubscribes; a closed
    /// channel is pruned on the next dispatch rather than requiring explicit removal.
    ///
    /// This is what a client needs to see events live. Polling [`EventBus::active_events`] cannot
    /// work: it reports which names have listeners, not what was dispatched, so it loses the
    /// arguments and cannot represent the same event firing twice.
    #[must_use]
    pub fn observe(
        &self,
        events: Option<Vec<&'static str>>,
    ) -> tokio::sync::mpsc::Receiver<ObservedEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(OBSERVER_QUEUE_CAPACITY);
        self.lock().observers.push(Observer {
            events,
            sink: tx,
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        });
        rx
    }

    /// Number of registered observers, for tests and diagnosis.
    #[must_use]
    pub fn observer_count(&self) -> usize {
        self.lock().observers.len()
    }

    /// Forwards one dispatch to every interested observer.
    ///
    /// Called after the listeners run, so an observer sees what actually happened rather than what
    /// was about to. Cheap when nothing is watching: the lock is taken, a length checked, released.
    fn announce(&self, name: &'static str, args: &impl serde::Serialize) {
        let payload = {
            let inner = self.lock();
            if inner.observers.is_empty() || !inner.observers.iter().any(|o| o.wants(name)) {
                return;
            }
            // Serialize once for all observers; a second observer must not cost a second pass.
            match serde_json::to_value(args) {
                Ok(value) => value,
                // A payload that will not serialize is a bug in the event definition, not a reason
                // to break a dispatch that already succeeded.
                Err(_) => return,
            }
        };

        let mut inner = self.lock();
        inner.observers.retain(|observer| {
            if !observer.wants(name) {
                return true;
            }
            match observer.sink.try_send(ObservedEvent {
                name,
                args: payload.clone(),
            }) {
                // Delivered.
                Ok(()) => true,
                // Full: this observer cannot keep up. Drop it rather than buffer without bound, and
                // count the loss so a caller can report why the stream stopped.
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    observer
                        .dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    false
                }
                // The receiver is gone.
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BusInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether `child` is `ancestor` or a descendant of it.
///
/// Keys are minted as `parent << 8 | n`, so an ancestor is a prefix and the test is a shift-and-
/// compare. Encoding the path in the number avoids storing a parent map and makes the check a
/// constant-time comparison rather than a walk.
fn scope_is_within(child: ScopeKey, ancestor: ScopeKey) -> bool {
    if ancestor == 0 {
        // The root scope owns everything.
        return true;
    }
    let mut cursor = child;
    loop {
        if cursor == ancestor {
            return true;
        }
        if cursor < 256 {
            // Reached the root without matching.
            return false;
        }
        cursor >>= 8;
    }
}

/// Mints a child scope key under `parent`.
#[must_use]
pub fn child_scope(parent: ScopeKey, index: u8) -> ScopeKey {
    if parent == 0 {
        // Depth one: the root is 0, so the first child starts at 1.
        ScopeKey::from(index).max(1)
    } else {
        (parent << 8) | ScopeKey::from(index)
    }
}
