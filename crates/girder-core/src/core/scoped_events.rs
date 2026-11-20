//! Scope admission for the event bus: who is allowed to receive a scoped dispatch, and the
//! part of that rule the bus cannot currently express.
//!
//! # What upstream `Scoped<T>` actually is
//!
//! `packages/core/scope/src/index.ts` — `Scoped<T>` is a **routing-only event receiver**, not a
//! data wrapper:
//!
//! ```text
//! declare const ScopedBrand: unique symbol
//! export type Scoped<T extends object> = object & { readonly [ScopedBrand]: T }
//! ```
//!
//! The brand exists only so the type parameter records which subject's events they are; the carrier
//! exposes none of that subject's properties. What actually routes is a Cordis **filter** installed
//! by [`scopeTarget`](https://github.com/deepseek-ai) (`scopeTarget(base, key)`):
//!
//! ```text
//! [CordisContext.filter](ctx) {
//!   if (baseFilter !== undefined && !baseFilter.call(base, ctx)) return false
//!   const tag = scopeOf(ctx)                      // the *dispatcher's* scope
//!   if (tag === undefined) return true            // untagged dispatch: everyone
//!   for (cursor = key; cursor; cursor = scopeParents.get(cursor))
//!     if (cursor === tag) return true             // listener key is tag or an ancestor
//!   return false
//! }
//! ```
//!
//! So the semantics are, precisely:
//!
//! 1. **Filtered by the dispatching context's scope tag, compared for identity.** A listener
//!    registered under a carrier carrying key `K` receives a dispatch performed through a context
//!    whose `scopeOf` is `T` when `T == K` **or** `K` is an ancestor of `T` along the declared
//!    parent chain.
//! 2. **Admission extends UP the chain, never down.** A parent-scoped listener sees every
//!    descendant's events (that is how one standing harness composition observes each agent
//!    composed under it); a child-scoped listener never sees a parent's event.
//! 3. **An untagged dispatch reaches every listener**, because `tag === undefined` returns `true`
//!    before the chain walk. Tagging narrows delivery; absence of a tag does not.
//! 4. **The unrelated base filter is preserved**, so scoping composes with whatever filter the
//!    subject already had.
//!
//! The ancestry it walks is the *declared* relation (`scopeParents`), written by
//! `bindScopeParent`/`rebind` under a cycle check — not derived from the key's value.
//!
//! # What `events.rs` already has
//!
//! `crates/girder-core/src/events.rs` (901 lines, five dispatch modes) already carries a partial
//! version of exactly this, so this module does not re-implement the relation:
//!
//! - `ScopeKey = u64` — the opaque routing identity, with `child_scope(parent, index)` minting
//!   children as `parent << 8 | index`. Cheap, allocation-free, and **ancestry encoded in the
//!   number**.
//! - `ListenerScope::{Global, Scoped(ScopeKey)}` — where a listener sits, reusing upstream's word
//!   for case 1 above ("`Global`" is how the bus spells upstream's untagged listener).
//! - `EventBus::on_scoped` / `EventBus::emit_scoped` — registration and dispatch, with the private
//!   `scope_is_within(child, ancestor)` doing the upward walk in the shift encoding.
//! - `EventBus::emit` — an unscoped dispatch, which admits **only** `Global` listeners.
//!
//! # The gap, precisely
//!
//! Four things upstream does are missing, and each one is a place where a caller can currently get
//! the wrong answer without the compiler or a test noticing.
//!
//! ## 1. Three of the five dispatch modes have no scoped form
//!
//! Upstream's 27 `Scoped<T>` event declarations are not all plain `emit`. Classified by their
//! declared signature:
//!
//! | shape | count | local scoped form | events |
//! |---|---|---|---|
//! | waterfall (`next`) | 10 | none | `agent/pre-step`, `agent/request`, `agent/request-error`, `approval/request`, `system-prompt/assemble`, `tools/execute`, `tools/post-execute`, `tools/pre-execute`, `tools/ptc-dispatch-log`, `user-questions/request` |
//! | awaited (`Promise<void> \| void`) | 3 | none | `agent/created`, `agent/turn-stopping`, `session/flush` |
//! | notify (`void`/`undefined`) | 14 | `on_scoped`/`emit_scoped` | `agent/assistant-stream`, `agent/disposed`, `agent/error`, `agent/inbox/{claimed,discarded,inserted}`, `agent/status`, `goal/changed`, `session/{created,disposed,event}`, `subagent/{start,end}`, `tools/result` |
//!
//! Locally, only the plain [`Event`](girder_core::Event) mode has `on_scoped`/`emit_scoped`.
//! [`Parallel`](girder_core::Parallel), [`Serial`](girder_core::Serial),
//! [`Bail`](girder_core::Bail), and [`Waterfall`](girder_core::Waterfall) have `on_*` with no scope
//! parameter and `parallel`/`serial`/`bail`/`waterfall` dispatch that snapshots **every** listener,
//! scoped ones included. Registering a scoped listener for a waterfall event is not expressible at
//! all today; if it were, `waterfall()` would deliver it to every agent.
//!
//! ## 2. Mutable, re-bindable ancestry is unreachable from the bus
//!
//! `scope_is_within` walks the *number*. That is exactly right for keys minted by `child_scope`
//! and nothing else: a key from another dispatcher, or one whose parent moved, walks a chain that
//! was never declared, and the walk stops on the heuristic `cursor < 256` rather than on a recorded
//! link. [`girder_core::ScopeGraph`] (in `agent_scope_gap.rs`) does hold the declared relation with
//! `rebind` and a shared cycle check — but the bus cannot consult it, and `ScopeGraph`'s keys are
//! its own `u64` unrelated to `crate::core::events::ScopeKey`.
//!
//! This is the gap [`Admission`] closes: the same `ScopeKey` type the bus already uses, resolved
//! through the declared [`ScopeGraph`], with a compile-time-total fallback to the shift encoding for
//! keys the graph has never seen.
//!
//! ## 3. An untagged dispatch admits nobody
//!
//! Upstream: `tag === undefined` ⇒ `true` ⇒ **every** listener receives it. Local `EventBus::emit`
//! admits only `Global`, deliberately (its comment: a scoped listener "would see events from
//! subtrees it never opted into"). The local choice is defensible for `emit`, but it is *not*
//! upstream's rule, and there is no local way to express upstream's rule for a caller that wants
//! it. [`Admission::from_dispatch`] takes `Option<ScopeKey>` for exactly this reason: `None` is a
//! real, distinct case that admits everyone.
//!
//! ## 4. There is no catalogue of which events are scoped
//!
//! Nothing local records *which* events carry `Scoped<T>`. The migration plan says 20; the actual
//! count in `packages/**/src/*.ts` is **27 declarations** (the plan's number predates the
//! `agent/inbox/*` and `user-questions/*` additions). [`SCOPED_EVENTS`] is that catalogue, so a
//! port can be checked against the real set instead of a remembered one.
//!
//! # What this module is not
//!
//! No Cordis and no `Fiber`: no carrier object, no `scopeOf(ctx)`, no effect ownership. Those are
//! context machinery and the design decision recorded for this port is not to emulate them — see
//! the `agent_scope_gap` module docs for the same argument. This module is the *admission
//! decision*, which is the part that has a right answer independent of where it runs.
//!
//! # How `events.rs` uses this
//!
//! [`EventBus::emit_scoped`](crate::EventBus::emit_scoped) asks [`Admission`] with
//! [`Ancestry::Encoded`](Ancestry::Encoded), which is the same walk `child_scope` mints. Waterfall,
//! parallel, serial, and bail still have no scoped form: a scoped listener cannot be registered
//! for those modes, so they do not consult this module. An untagged [`EventBus::emit`](crate::EventBus::emit)
//! still admits only global listeners; [`Admission::unscoped`] is the upstream rule for a caller
//! that wants every listener, and nothing on the bus asks for that.

use ::girder_core::{ListenerScope, ScopeGraph, ScopeKey, child_scope};

/// How a scoped listener's ancestry is resolved.
///
/// Two sources of truth exist for "is this key below that one", and a dispatcher must know which
/// one it is using, because they disagree on purpose:
///
/// - [`Declared`](Admission::Declared) — the [`ScopeGraph`] relation, which can be re-bound and is
///   cycle-checked. An unknown key falls back to the numeric encoding, so a scope minted by
///   `child_scope` alone still works.
/// - [`Encoded`](Admission::Encoded) — the shift encoding in `events.rs`, for a bus that mints keys
///   and never re-parents, which is the common case and needs no table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ancestry<'a> {
    /// The declared relation, with a numeric fallback for keys it has never seen.
    Declared(&'a ScopeGraph),
    /// The numeric encoding alone: `child_scope`'s `parent << 8 | index`.
    Encoded,
}

impl Ancestry<'_> {
    /// Whether `candidate` is `ancestor` or sits below it.
    ///
    /// The one direction the event bus admits: a listener tagged `ancestor` receives a descendant's
    /// dispatch, never the reverse.
    #[must_use]
    pub fn is_within(self, candidate: ScopeKey, ancestor: ScopeKey) -> bool {
        // The root scope owns everything. Checked before either encoding because both stop their
        // walk at the root without ever comparing against it, and because it is a property of the
        // key encoding rather than of any one table.
        if ancestor == 0 {
            return true;
        }
        match self {
            Self::Encoded => encoded_is_within(candidate, ancestor),
            Self::Declared(graph) => {
                // A key that participates in the declared relation is answered by the graph and
                // *only* by the graph. Falling through to the encoding here would re-admit a
                // re-bound key through the digits it kept from its old parent — the precise case
                // `rebind` exists for, silently undone.
                if graph.parent_of(candidate).is_some() {
                    return graph.is_within(candidate, ancestor);
                }
                // The graph only knows keys someone bound. A key minted by `child_scope` and never
                // bound is still a child of its prefix, and refusing it here would make the
                // declared path *narrower* than the encoded one for the same key pair — a
                // difference no caller asked for.
                encoded_is_within(candidate, ancestor)
            }
        }
    }
}

/// Whether `child` is `ancestor` or below it, by the numeric encoding alone.
///
/// Mirrors `events.rs`'s private `scope_is_within` exactly, including the `< 256` floor: the root is
/// `0`, the first level is `1..=255`, and every deeper level shifts by eight bits. Kept as a
/// separate function here only because the bus's copy is private; when the wiring lands this can
/// delegate to it instead of duplicating four lines.
#[must_use]
pub fn encoded_is_within(child: ScopeKey, ancestor: ScopeKey) -> bool {
    if ancestor == 0 {
        // The root scope owns everything. Also the only correct stop condition for a key at depth
        // one, whose parent *is* the root and which therefore never enters the loop below.
        return true;
    }
    let mut cursor = child;
    loop {
        if cursor == ancestor {
            return true;
        }
        if cursor < 256 {
            // Reached the root without matching: depth-one keys and `0` both stop here.
            return false;
        }
        cursor >>= 8;
    }
}

/// One dispatch's admission rule: a scope tag plus the ancestry that interprets it.
///
/// Built once per dispatch and asked once per listener, so the [`ScopeGraph`] is borrowed rather
/// than consulted through a lock per listener. `Copy` because it holds only a reference and an
/// `Option<u64>`, which makes it usable inside the dispatch loop without cloning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission<'a> {
    tag: Option<ScopeKey>,
    ancestry: Ancestry<'a>,
}

impl<'a> Admission<'a> {
    /// Admission for a dispatch carrying `tag`, resolved through `ancestry`.
    ///
    /// `Some(key)` is a scoped dispatch; `None` is an **untagged** one, which upstream admits to
    /// every listener. Passing `None` rather than a sentinel is deliberate: the untagged case is a
    /// different rule, not a different key, and a sentinel like `0` would silently mean "the root
    /// scope" instead — which admits everyone only by accident of the encoding.
    #[must_use]
    pub const fn new(tag: Option<ScopeKey>, ancestry: Ancestry<'a>) -> Self {
        Self { tag, ancestry }
    }

    /// Admission for a dispatch scoped to `tag`.
    #[must_use]
    pub const fn scoped(tag: ScopeKey, ancestry: Ancestry<'a>) -> Self {
        Self::new(Some(tag), ancestry)
    }

    /// Admission that reaches every listener, scoped or not.
    ///
    /// Upstream's `tag === undefined` case. Use it for a genuinely harness-wide dispatch, not as a
    /// default: a caller that has a tag and drops it broadcasts to every agent's listeners.
    #[must_use]
    pub const fn unscoped(ancestry: Ancestry<'a>) -> Self {
        Self::new(None, ancestry)
    }

    /// The dispatch tag, or `None` for an untagged dispatch.
    #[must_use]
    pub const fn tag(&self) -> Option<ScopeKey> {
        self.tag
    }

    /// The ancestry this admission resolves through.
    #[must_use]
    pub const fn ancestry(&self) -> Ancestry<'a> {
        self.ancestry
    }

    /// Whether a listener registered at `listener` should receive this dispatch.
    ///
    /// The whole rule, in upstream's order:
    ///
    /// - [`ListenerScope::Global`] always receives it, tagged or not.
    /// - An **untagged** dispatch (`tag == None`) receives everyone, scoped listeners included.
    /// - Otherwise a scoped listener receives it when its key is the tag or an ancestor of it.
    ///
    /// Order matters in one place: the untagged check comes **before** the scoped comparison, so an
    /// untagged dispatch cannot be filtered by ancestry at all. Checking the tag first would make
    /// `None` compare unequal to every key and admit only global listeners, which is `EventBus::emit`'s
    /// local behaviour and not the upstream rule this type exists to express.
    #[must_use]
    pub fn admits(&self, listener: ListenerScope) -> bool {
        match listener {
            ListenerScope::Global => true,
            ListenerScope::Scoped(key) => match self.tag {
                None => true,
                Some(tag) => self.ancestry.is_within(tag, key),
            },
        }
    }

    /// `admits(listener)`, negated, for callers that filter rather than branch.
    #[must_use]
    pub fn excludes(&self, listener: ListenerScope) -> bool {
        !self.admits(listener)
    }

    /// The admitted subset of `listeners`, in their original order.
    ///
    /// Order is preserved rather than filtered in place so a caller can snapshot once and dispatch
    /// from the result. For the ordering-sensitive modes — `serial`, `bail`, and `waterfall` — the
    /// order *is* the semantics, and `Prepend` registration has to survive admission.
    #[must_use]
    pub fn admit<'s>(
        &self,
        listeners: impl IntoIterator<Item = &'s ListenerScope>,
    ) -> Vec<ListenerScope> {
        listeners
            .into_iter()
            .filter(|scope| self.admits(**scope))
            .copied()
            .collect()
    }
}

/// One upstream event that declares `Scoped<T>`, with the subject type and dispatch shape.
///
/// The subject is a `&'static str` rather than a type because these are upstream names recorded for
/// a port, not local types: `"Agent"` means the carrier is `Scoped<Agent>`. Keeping them as text
/// lets the table be read against `packages/**/src/*.ts` directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopedEvent {
    /// Dispatch name in the upstream `domain/name` spelling.
    pub name: &'static str,
    /// The `T` in `Scoped<T>`.
    pub subject: &'static str,
    /// How it is dispatched, which decides whether a scoped form is expressible locally today.
    pub shape: DispatchShape,
}

impl ScopedEvent {
    /// Whether this event is dispatched as a waterfall (has a `next` in its signature).
    ///
    /// The distinction that matters most: a scoped waterfall is the case the local bus cannot
    /// express at all, and these are the ten events where delivering to a sibling agent is a real
    /// bug rather than a stray notification — `tools/execute` and `approval/request` run in *that*
    /// agent's context.
    #[must_use]
    pub const fn is_waterfall(&self) -> bool {
        self.shape.is_waterfall()
    }
}

/// The dispatch shape an upstream scoped event declares.
///
/// Three, not two, because the `Scoped<T>` declarations differ in whether their listeners return
/// nothing, return a promise someone awaits, or take a `next`. Those are the local `emit`,
/// `parallel`/`serial`, and `waterfall` modes respectively — and they differ in whether the local
/// bus has a scoped form at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchShape {
    /// Notify-only: returns `void`/`undefined`, nobody awaits it. The local [`Event`](girder_core::Event)
    /// mode, which is the one with `on_scoped`/`emit_scoped`.
    Notify,
    /// Awaited but unshaped: the listener returns `Promise<void> | void` and every listener is
    /// settled. The local [`Parallel`](girder_core::Parallel) mode — which has **no** scoped form,
    /// so these need [`Support::AdmitOutsideBus`].
    Awaited,
    /// Waterfall: takes a `next` continuation. The local [`Waterfall`](girder_core::Waterfall) mode,
    /// where a globally registered layer would have to filter itself; no scoped form exists.
    Waterfall,
}

impl DispatchShape {
    /// Whether this shape is dispatched as a waterfall (takes a `next`).
    ///
    /// The distinction that matters most: a scoped waterfall is the case the local bus cannot
    /// express at all, and these are the ten events where delivering to a sibling agent is a real
    /// bug rather than a stray notification — `tools/execute` and `approval/request` run in *that*
    /// agent's context.
    #[must_use]
    pub const fn is_waterfall(self) -> bool {
        matches!(self, Self::Waterfall)
    }
}

/// Every upstream event declaring `Scoped<T>`, classified.
///
/// Derived from `packages/**/src/**/*.ts` (excluding `*.d.ts` and the generated `api-catalog`), by
/// matching `'name'(this: Scoped<T>` including declarations split across lines. 27 entries: the
/// migration plan's "20" predates `agent/inbox/{claimed,discarded,inserted}`,
/// `approval/request`, `user-questions/request`, and `goal/changed`.
///
/// `goal/changed` spells its type as `import('@deepseek-ai/dsh-scope').Scoped<Agent>`; it is the
/// same declaration and is included once.
pub const SCOPED_EVENTS: &[ScopedEvent] = &[
    ScopedEvent {
        name: "agent/assistant-stream",
        subject: "Agent",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "agent/created",
        subject: "Agent",
        shape: DispatchShape::Awaited,
    },
    ScopedEvent {
        name: "agent/disposed",
        subject: "Agent",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "agent/error",
        subject: "Agent",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "agent/inbox/claimed",
        subject: "Agent",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "agent/inbox/discarded",
        subject: "Agent",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "agent/inbox/inserted",
        subject: "Agent",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "agent/pre-step",
        subject: "Agent",
        shape: DispatchShape::Waterfall,
    },
    ScopedEvent {
        name: "agent/request",
        subject: "Agent",
        shape: DispatchShape::Waterfall,
    },
    ScopedEvent {
        name: "agent/request-error",
        subject: "Agent",
        shape: DispatchShape::Waterfall,
    },
    ScopedEvent {
        name: "agent/status",
        subject: "Agent",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "agent/turn-stopping",
        subject: "Agent",
        shape: DispatchShape::Awaited,
    },
    ScopedEvent {
        name: "approval/request",
        subject: "Agent",
        shape: DispatchShape::Waterfall,
    },
    ScopedEvent {
        name: "goal/changed",
        subject: "Agent",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "session/created",
        subject: "Session",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "session/disposed",
        subject: "Session",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "session/event",
        subject: "Session",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "session/flush",
        subject: "Session",
        shape: DispatchShape::Awaited,
    },
    ScopedEvent {
        name: "subagent/end",
        subject: "SubagentRuntime",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "subagent/start",
        subject: "SubagentRuntime",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "system-prompt/assemble",
        subject: "SystemPrompt",
        shape: DispatchShape::Waterfall,
    },
    ScopedEvent {
        name: "tools/execute",
        subject: "ToolsService",
        shape: DispatchShape::Waterfall,
    },
    ScopedEvent {
        name: "tools/post-execute",
        subject: "ToolsService",
        shape: DispatchShape::Waterfall,
    },
    ScopedEvent {
        name: "tools/pre-execute",
        subject: "ToolsService",
        shape: DispatchShape::Waterfall,
    },
    ScopedEvent {
        name: "tools/ptc-dispatch-log",
        subject: "ToolsService",
        shape: DispatchShape::Waterfall,
    },
    ScopedEvent {
        name: "tools/result",
        subject: "ToolsService",
        shape: DispatchShape::Notify,
    },
    ScopedEvent {
        name: "user-questions/request",
        subject: "Agent",
        shape: DispatchShape::Waterfall,
    },
];

/// The upstream declaration for `name`, if it carries a scope.
///
/// Linear over 27 entries, which is faster than hashing for a table this size and keeps the
/// constant a plain `&'static [T]` that a reader can diff against upstream.
#[must_use]
pub fn scoped_event(name: &str) -> Option<&'static ScopedEvent> {
    SCOPED_EVENTS.iter().find(|event| event.name == name)
}

/// Whether `name` is dispatched as a scoped waterfall upstream.
///
/// The set to audit first when wiring the bus: these cannot be expressed locally today, and a
/// blanket "call the global dispatch" fallback delivers one agent's tool execution to another's
/// listeners.
#[must_use]
pub fn is_scoped_waterfall(name: &str) -> bool {
    scoped_event(name).is_some_and(ScopedEvent::is_waterfall)
}

/// What the local bus can express for one upstream scoped event.
///
/// A three-way answer rather than a boolean, because the middleware is different in each case: a
/// fully supported event can use [`Admission`] directly, a partially supported one needs the scope
/// checked outside the bus, and an unsupported one has no path at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// `on_scoped` + `emit_scoped` exist: plain notifying events.
    Native,
    /// The mode exists but has no scoped form; [`Admission`] must be consulted by the caller
    /// before dispatch, and the listener must be registered globally.
    AdmitOutsideBus,
    /// The mode has no scoped form *and* is a waterfall, so a globally-registered layer would have
    /// to filter itself. Expressing it requires an `events.rs` change.
    Unsupported,
}

/// How the local bus can serve `name` today.
///
/// The mapping is mechanical, one arm per shape:
///
/// | shape | local mode | scoped form | verdict |
/// |---|---|---|---|
/// | [`Notify`](DispatchShape::Notify) | `Event` | `on_scoped`/`emit_scoped` | [`Support::Native`] |
/// | [`Awaited`](DispatchShape::Awaited) | `Parallel` | none | [`Support::AdmitOutsideBus`] |
/// | [`Waterfall`](DispatchShape::Waterfall) | `Waterfall` | none | [`Support::Unsupported`] |
#[must_use]
pub fn support(name: &str) -> Option<Support> {
    scoped_event(name).map(|event| match event.shape {
        DispatchShape::Notify => Support::Native,
        DispatchShape::Awaited => Support::AdmitOutsideBus,
        DispatchShape::Waterfall => Support::Unsupported,
    })
}

/// The upstream events of `shape`, sorted.
fn events_of_shape(shape: DispatchShape) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = SCOPED_EVENTS
        .iter()
        .filter(|event| event.shape == shape)
        .map(|event| event.name)
        .collect();
    names.sort_unstable();
    names
}

/// The upstream scoped events that can use `admit` outside the bus but have no scoped dispatch,
/// sorted.
///
/// The `Awaited` group: every listener is settled, so a caller can snapshot the scoped entries,
/// ask [`Admission`], and await the admitted ones itself — which is exactly what upstream's
/// `session/flush` already does by hand. No `events.rs` change is strictly required for these.
#[must_use]
pub fn admit_outside_bus_events() -> Vec<&'static str> {
    events_of_shape(DispatchShape::Awaited)
}

/// The upstream scoped events with no local path, sorted.
///
/// The work list for the wiring change: these are the ones where a missing scoped form means a
/// sibling agent's layer runs on another agent's event, and where the layer's position in the
/// chain is itself the semantics — so filtering it after the fact is not equivalent.
#[must_use]
pub fn unsupported_upstream_events() -> Vec<&'static str> {
    events_of_shape(DispatchShape::Waterfall)
}

/// The upstream scoped events the bus already routes correctly, sorted.
#[must_use]
pub fn native_upstream_events() -> Vec<&'static str> {
    events_of_shape(DispatchShape::Notify)
}

/// The scope key a child of `parent` should carry, for a bus that mints keys by index.
///
/// Re-exported rather than re-implemented: two encodings of the same number would be one too many,
/// and a caller minting keys through this name cannot drift from the one `events.rs` walks.
#[must_use]
pub fn mint_child(parent: ScopeKey, index: u8) -> ScopeKey {
    child_scope(parent, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A root and two siblings, the shape an agent tree has.
    fn tree() -> (ScopeKey, ScopeKey, ScopeKey) {
        let root = child_scope(0, 1);
        (root, child_scope(root, 1), child_scope(root, 2))
    }

    #[test]
    fn a_listener_sees_its_own_subtree_and_never_a_siblings() {
        // The dispatch tag decides. A listener at `root` observes every descendant's dispatch; a
        // listener at `a` observes only `a`'s, and never `root`'s or `b`'s.
        let (root, a, b) = tree();

        let at_root = Admission::scoped(a, Ancestry::Encoded);
        assert!(
            at_root.admits(ListenerScope::Scoped(root)),
            "an ancestor listener observes a descendant's dispatch"
        );

        let at_a = Admission::scoped(root, Ancestry::Encoded);
        assert!(
            !at_a.admits(ListenerScope::Scoped(a)),
            "a descendant listener never observes its ancestor's dispatch"
        );

        let sibling = Admission::scoped(b, Ancestry::Encoded);
        assert!(!sibling.admits(ListenerScope::Scoped(a)));
        assert!(sibling.admits(ListenerScope::Global));
    }

    #[test]
    fn a_sibling_is_not_admitted_in_either_encoding() {
        let (_root, a, b) = tree();
        let graph = ScopeGraph::new();
        for ancestry in [Ancestry::Encoded, Ancestry::Declared(&graph)] {
            let admission = Admission::scoped(a, ancestry);
            assert!(admission.admits(ListenerScope::Scoped(a)), "{ancestry:?}");
            assert!(!admission.admits(ListenerScope::Scoped(b)), "{ancestry:?}");
        }
    }

    #[test]
    fn admission_never_extends_down() {
        let (root, a, _b) = tree();
        let admission = Admission::scoped(root, Ancestry::Encoded);
        assert!(
            !admission.admits(ListenerScope::Scoped(a)),
            "a child's listener must not see the parent's dispatch"
        );
    }

    #[test]
    fn an_untagged_dispatch_admits_scoped_listeners() {
        // Upstream's `tag === undefined` rule, which differs from `EventBus::emit`.
        let (_root, a, b) = tree();
        let admission = Admission::unscoped(Ancestry::Encoded);
        assert!(admission.admits(ListenerScope::Scoped(a)));
        assert!(admission.admits(ListenerScope::Scoped(b)));
        assert!(admission.admits(ListenerScope::Global));
        assert_eq!(admission.tag(), None);
    }

    #[test]
    fn the_declared_graph_can_move_a_key() {
        // The property the encoding cannot express: `a` keeps its identity and adopts a new parent,
        // after which the *old* chain no longer admits it and the new one does.
        let (root, a, _b) = tree();
        let mut graph = ScopeGraph::new();
        let outer = child_scope(0, 7);
        graph.bind(a, outer).expect("first bind");

        let declared = Admission::scoped(a, Ancestry::Declared(&graph));
        assert!(declared.admits(ListenerScope::Scoped(outer)));
        assert!(
            !declared.admits(ListenerScope::Scoped(root)),
            "the numeric prefix still matches, but the declared relation does not"
        );

        // The encoded view of the same keys disagrees, which is the whole reason the two exist.
        let encoded = Admission::scoped(a, Ancestry::Encoded);
        assert!(encoded.admits(ListenerScope::Scoped(root)));
        assert!(!encoded.admits(ListenerScope::Scoped(outer)));
    }

    #[test]
    fn an_unbound_key_still_walks_its_encoding() {
        // A key minted by `child_scope` and never bound must not become narrower just because a
        // graph was supplied.
        let (root, a, _b) = tree();
        let graph = ScopeGraph::new();
        let admission = Admission::scoped(a, Ancestry::Declared(&graph));
        assert!(graph.is_empty());
        assert!(admission.admits(ListenerScope::Scoped(root)));
    }

    #[test]
    fn admit_keeps_registration_order() {
        // `serial`, `bail`, and `waterfall` depend on order, so filtering must not sort.
        let (_root, a, b) = tree();
        let listeners = [
            ListenerScope::Scoped(a),
            ListenerScope::Global,
            ListenerScope::Scoped(b),
        ];
        let admitted = Admission::scoped(a, Ancestry::Encoded).admit(&listeners);
        assert_eq!(
            admitted,
            vec![ListenerScope::Scoped(a), ListenerScope::Global]
        );
    }

    #[test]
    fn excludes_is_the_negation_of_admits() {
        let (root, a, b) = tree();
        let admission = Admission::scoped(a, Ancestry::Encoded);
        for listener in [
            ListenerScope::Global,
            ListenerScope::Scoped(root),
            ListenerScope::Scoped(a),
            ListenerScope::Scoped(b),
        ] {
            assert_eq!(admission.excludes(listener), !admission.admits(listener));
        }
    }

    #[test]
    fn the_catalogue_matches_the_upstream_count_and_is_unique() {
        assert_eq!(SCOPED_EVENTS.len(), 27);
        let mut names: Vec<&str> = SCOPED_EVENTS.iter().map(|event| event.name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "no event may be listed twice");
    }

    #[test]
    fn the_catalogue_partitions_into_the_three_shapes() {
        assert_eq!(native_upstream_events().len(), 14, "notify");
        assert_eq!(admit_outside_bus_events().len(), 3, "awaited");
        assert_eq!(unsupported_upstream_events().len(), 10, "waterfall");
        assert_eq!(
            native_upstream_events().len()
                + admit_outside_bus_events().len()
                + unsupported_upstream_events().len(),
            SCOPED_EVENTS.len()
        );
    }

    #[test]
    fn the_awaited_group_is_the_three_that_return_a_promise() {
        // These have no scoped dispatch form either, but every listener is settled, so a caller can
        // filter and await them itself — which upstream's own `session/flush` already does.
        assert_eq!(
            admit_outside_bus_events(),
            vec!["agent/created", "agent/turn-stopping", "session/flush"]
        );
        assert_eq!(support("session/flush"), Some(Support::AdmitOutsideBus));
    }

    #[test]
    fn the_waterfall_set_is_the_one_that_cannot_be_wired_today() {
        // Named explicitly rather than counted, because these are the ten where a sibling agent
        // running another agent's listener is a real bug.
        assert_eq!(
            unsupported_upstream_events(),
            vec![
                "agent/pre-step",
                "agent/request",
                "agent/request-error",
                "approval/request",
                "system-prompt/assemble",
                "tools/execute",
                "tools/post-execute",
                "tools/pre-execute",
                "tools/ptc-dispatch-log",
                "user-questions/request",
            ]
        );
        assert!(is_scoped_waterfall("approval/request"));
        assert!(!is_scoped_waterfall("agent/created"));
        assert!(!is_scoped_waterfall("not-an-event"));
    }

    #[test]
    fn an_unknown_event_has_no_support_claim() {
        assert_eq!(scoped_event(""), None);
        assert_eq!(support("session/created"), Some(Support::Native));
        assert_eq!(support("tools/execute"), Some(Support::Unsupported));
        assert_eq!(
            support("agent-preset/selected"),
            None,
            "not scoped upstream"
        );
    }

    #[test]
    fn mint_child_agrees_with_the_bus_encoding() {
        assert_eq!(mint_child(0, 1), child_scope(0, 1));
        let root = mint_child(0, 3);
        assert_eq!(mint_child(root, 4), child_scope(root, 4));
    }
}
