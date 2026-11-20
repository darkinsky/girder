//! Agent-scope algebra: the scope parent chain and the consumed-work account of a turn log.
//!
//! # Where this comes from
//!
//! Two of `packages/core`'s files carry pure logic with no Cordis dependency, and neither had a
//! local counterpart:
//!
//! - `scope/src/index.ts` — the scope *parent relation*: `bindScopeParent`, `scopeParentOf`,
//!   `scopeChainOf`, and the cycle check shared by the bind and the re-link.
//! - `agent/src/consumed-work.ts` — `foldConsumedWork`, the fold that answers "did this log
//!   actually account for the work it took?"
//!
//! # Why the scope relation, when `events.rs` already has `ScopeKey`
//!
//! `girder_core::core::events` does answer "is this listener in scope?" — but by encoding ancestry
//! into the *number*: [`girder_core::child_scope`] shifts a parent left by eight bits and or's in
//! the child index. That encoding is cheap and needs no allocation, and it is exactly right for a
//! dispatcher that mints keys in a tree and only ever walks upward.
//!
//! It cannot express the relation upstream actually declares, in two ways that matter:
//!
//! 1. **It is not revisable.** Upstream's `ScopeParentBinding::rebind` exists because a blank
//!    session is recomposed under a different preset: the agent scope keeps its identity and
//!    adopts a new parent. In the shift encoding the parent is *inside the key*, so re-parenting
//!    would mean minting a new key — which detaches every listener registered under the old one.
//! 2. **It silently assumes a full tree.** Any two keys share a numeric prefix, so the walk stops
//!    on the "root" heuristic `cursor < 256` rather than on a recorded parent link. A key minted
//!    by a different dispatcher, or a reparented one, walks a chain that was never declared.
//!
//! So this module keeps the relation in an explicit table, asymmetrically: one write path
//! ([`bind`]), one privileged re-link path ([`Binding::rebind`]) that only the original binder
//! holds, and reads that cannot mutate. The cycle check is shared by both writers, because a
//! re-link that closes a loop poisons every consumer just as thoroughly as a bind that does.
//!
//! # Why the consumed-work fold, when turn closure is already decided
//!
//! `girder_core::core::turn_closure` answers "is a turn closed?" and [`girder_core::SessionEvent`]
//! records the answering boundaries. Neither can answer the question a cancellation actually
//! asks: *was the work this log took ever run?* Two spellings of the same log look identical there:
//!
//! ```text
//! turn/start, turn/end{completed}          <- a no-op turn: nothing was claimed, nothing to run
//! turn/start, <claim>, turn/end{completed} <- a turn whose claimed work was rewritten away unrun
//! ```
//!
//! The missing fact is not on the turn events at all — it is the inbox's own record. A claim is an
//! `agent/inbox/spliced` with a `removedCount` taken inside an open turn; a *cancellation* is the
//! same event with `outcome: "canceled"`, and it is the only account of input that a cancellation
//! took before any turn could open over it. Folding the two together is what separates
//! "finished its work" from "cut short" from "dropped before it started".
//!
//! # Why the fold takes a borrowed event view instead of `&[SessionEvent]`
//!
//! [`girder_core::SessionEvent`] is a closed enum, and `agent/inbox/spliced` is not a variant of
//! it — see the note in `girder_core::session::queue_rpc`, which reports the same absence from
//! the other side. Folding `&[SessionEvent]` would force this module to invent a variant (a shape
//! in every log that no reader understands) or to drop the splice records (the very fact the fold
//! exists to read). [`Event`] is therefore a borrowed, minimal view: a caller that holds decoded
//! `SessionEvent`s maps the three boundary cases and ignores the rest, and a caller folding a raw
//! log maps the splice too. The fold stays pure and one-pass over whatever it is given.
//!
//! # What this module deliberately does not do
//!
//! - No Cordis. `createScope`, `scopeTarget`, `Scope`, `rawDispose`, and the `ScopedLayers`
//!   effect ownership are all context/fiber machinery; that is the design decision recorded for
//!   this port, and nothing here pretends to be it.
//! - No projection registry. Upstream's `inboxProjectionSchema` decodes splices through zod at
//!   the storage boundary; a local fold consumes already-decoded facts.
//! - No lock and no interior mutability in the relation. [`ScopeGraph`] is a plain value: a caller
//!   that needs to share one across threads wraps it, and the borrow rules then make the
//!   privileged re-link obvious at the call site.

use std::collections::BTreeMap;

use girder_core::TurnEndReason;

/// An opaque identity used to route scoped events and to name a scope's place in the chain.
///
/// Numeric for the same reason the event bus's `ScopeKey` is: these are compared and
/// stored, never dereferenced. Unlike that encoding, a key here carries no ancestry — the parent
/// is a separate link that can move.
pub type ScopeKey = u64;

/// Why a scope relation was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeError {
    /// The key is already linked to a parent, so only the binding that made that link may move it.
    ///
    /// Re-binding from the outside would let any holder quietly re-parent a live subtree; upstream
    /// keeps the write path single so a scope's ancestry has exactly one owner.
    AlreadyBound {
        /// The key whose parent was already set.
        key: ScopeKey,
    },

    /// The link would close a loop, which every chain walker would then run forever.
    ///
    /// Reported instead of thrown because the caller can only be told; a cycle is a programming
    /// error in the composition that asked for it, and naming the two keys is what makes it
    /// findable.
    Cycle {
        /// The child key the link would have given a parent.
        key: ScopeKey,
        /// The proposed parent, which is already inside the child's own subtree.
        parent: ScopeKey,
    },
}

impl std::fmt::Display for ScopeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyBound { key } => write!(
                formatter,
                "scope key {key} is already bound to a parent; re-linking requires the binding from the original bind"
            ),
            Self::Cycle { key, parent } => write!(
                formatter,
                "binding scope key {key} under {parent} would form a cycle"
            ),
        }
    }
}

impl std::error::Error for ScopeError {}

/// The scope parent relation: one link per key, and no link for a root.
///
/// Reads never create a link, which is what lets a caller ask about a scope that was never
/// minted without the question itself changing the answer. Writes go through exactly two paths —
/// [`ScopeGraph::bind`] for the first link and [`Binding::rebind`] for the one privileged move —
/// and both run the same cycle check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeGraph {
    parents: BTreeMap<ScopeKey, ScopeKey>,
}

impl ScopeGraph {
    /// Creates an empty relation: every key is a root.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds `parent` as `key`'s enclosing scope, once.
    ///
    /// # Errors
    ///
    /// [`ScopeError::AlreadyBound`] when `key` already has a parent — there is no open re-link
    /// path, so an ancestry cannot be moved by anyone but the binder, who alone receives the
    /// [`Binding`]. [`ScopeError::Cycle`] when `parent` is `key` itself or already descends from
    /// it, because every chain consumer walks parents to the root.
    pub fn bind(&mut self, key: ScopeKey, parent: ScopeKey) -> Result<Binding, ScopeError> {
        if self.parents.contains_key(&key) {
            return Err(ScopeError::AlreadyBound { key });
        }
        self.link(key, parent)?;
        Ok(Binding { key, parent })
    }

    /// Reads one key's enclosing scope.
    ///
    /// `None` for a root scope *and* for a key that was never bound: the question "what is this
    /// key's parent" has the same answer either way, and distinguishing them would leak whether a
    /// scope exists.
    #[must_use]
    pub fn parent_of(&self, key: ScopeKey) -> Option<ScopeKey> {
        self.parents.get(&key).copied()
    }

    /// The chain from `key` to its root ancestor, nearest first.
    ///
    /// `[key, parent, grandparent, …]` — the order a scoped-layers merge reverses to give the
    /// nearest scope the final word. Empty for `None`, and a lone key for a root.
    #[must_use]
    pub fn chain_of(&self, key: Option<ScopeKey>) -> Vec<ScopeKey> {
        let mut chain = Vec::new();
        let mut cursor = key;
        while let Some(current) = cursor {
            chain.push(current);
            cursor = self.parents.get(&current).copied();
        }
        chain
    }

    /// Whether `candidate` is `ancestor` or sits below it in the chain.
    ///
    /// This is the event-admission direction: a listener tagged with an ancestor receives a
    /// descendant's dispatch, never the reverse. Expressed through [`ScopeGraph::chain_of`] so
    /// there is one walk, not two.
    #[must_use]
    pub fn is_within(&self, candidate: ScopeKey, ancestor: ScopeKey) -> bool {
        self.chain_of(Some(candidate)).contains(&ancestor)
    }

    /// How many keys are bound.
    #[must_use]
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    /// Whether no key is bound — every key is a root.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    /// The shared cycle-checked write.
    ///
    /// Walking from the proposed parent upward must never reach `key`: if it does, `parent` is
    /// already inside `key`'s own subtree and the link closes a loop. A self-link is the
    /// degenerate case of the same rule, caught by the first iteration.
    fn link(&mut self, key: ScopeKey, parent: ScopeKey) -> Result<(), ScopeError> {
        let mut cursor = Some(parent);
        while let Some(current) = cursor {
            if current == key {
                return Err(ScopeError::Cycle { key, parent });
            }
            cursor = self.parents.get(&current).copied();
        }
        self.parents.insert(key, parent);
        Ok(())
    }
}

/// The privileged handle to move one key's parent link.
///
/// Returned only by [`ScopeGraph::bind`], so possession *is* the permission. It is deliberately
/// not `Copy`: handing out two handles to one link would make "the original binder" a fiction.
///
/// The move it permits is the blank-session recompose contract — the agent scope keeps its
/// identity while adopting the new preset — and it is valid only while nothing produced under the
/// old parent is retained. The relation cannot see what a session logged, so that obligation stays
/// with the holder, which is why this type documents it rather than checking it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    key: ScopeKey,
    parent: ScopeKey,
}

impl Binding {
    /// The key this binding governs.
    #[must_use]
    pub fn key(&self) -> ScopeKey {
        self.key
    }

    /// The parent this binding installed.
    #[must_use]
    pub fn parent(&self) -> ScopeKey {
        self.parent
    }

    /// Re-links the governed key to a different parent, with the same cycle check as the bind.
    ///
    /// # Errors
    ///
    /// [`ScopeError::Cycle`] when `parent` is the governed key itself or already descends from it.
    /// The binding is left unconsumed on failure, so a refused re-link does not drop the only
    /// handle to the link.
    pub fn rebind(&mut self, graph: &mut ScopeGraph, parent: ScopeKey) -> Result<(), ScopeError> {
        graph.link(self.key, parent)?;
        self.parent = parent;
        Ok(())
    }
}

// ---- consumed work ----------------------------------------------------------------------------

/// One step of a log, as the consumed-work fold reads it.
///
/// A borrowed view rather than a decoded event because the splice fact this fold depends on has no
/// [`girder_core::SessionEvent`] variant (see the module docs). Every variant other than
/// [`Event::InboxSpliced`] is a boundary the log already models, so a caller with decoded events
/// maps them one-for-one and ignores the rest.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event<'a> {
    /// A turn began.
    TurnStart {
        /// Turn number.
        turn: u64,
    },

    /// A step began: the turn reached a model call.
    StepStart {
        /// Turn it belongs to.
        turn: u64,
    },

    /// A turn ended, and why.
    TurnEnd {
        /// Turn number.
        turn: u64,
        /// The recorded ending.
        reason: &'a TurnEndReason,
    },

    /// The inbox was spliced, which is where a claim and a cancellation are both recorded.
    InboxSpliced {
        /// How many pending messages the splice removed.
        ///
        /// `None` is upstream's absent `removedCount` — a pure insertion. An insertion alone is
        /// not a claim: only removal takes work out of the queue.
        removed: Option<usize>,
        /// Why the removed messages left, when the writer said.
        ///
        /// `Canceled` is the writer discarding them on purpose; `None` is the loop consuming them.
        outcome: SpliceOutcome,
        /// How many messages the same splice put back.
        ///
        /// A replacement keeps the work pending under a new identity, so only a cancellation that
        /// leaves nothing behind actually drops it.
        inserted: usize,
    },
}

/// Why an inbox splice's removals left the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpliceOutcome {
    /// The loop consumed them at a step boundary — the normal claim.
    Consumed,
    /// The writer discarded them, and they will never run.
    Canceled,
}

/// How one agent log accounts for the work it consumed.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ConsumedWork {
    /// The latest closed turn that accounts for consumed work.
    ///
    /// `Some` for a turn that entered a model step, or one that claimed inbox input and then
    /// failed, was stopped, or was rejected. `None` when no turn closed over any work.
    pub end: Option<EndedTurn>,

    /// Whether accepted work was cancelled out of the inbox, unrun, *after* that turn.
    ///
    /// This is the only account of input a cancellation took before any turn could open over it:
    /// no `turn/end` describes it, which is exactly why a fold over boundaries alone cannot see it.
    pub dropped_unrun: bool,
}

/// The closing record of the turn that accounts for consumed work.
#[derive(Clone, Debug, PartialEq)]
pub struct EndedTurn {
    /// Which turn closed.
    pub turn: u64,
    /// Why it ended.
    pub reason: TurnEndReason,
}

/// Whether a turn that consumed input but never reached a step ends in a way that accounts for it.
///
/// Only `Completed` does not: it had nothing left to run once its claim was rewritten away. Every
/// other ending accounts for the input — a `Blocked` end is that input's ending too, because the
/// pre-step rejection that produced it discarded the claimed messages.
///
/// The fallback is `true` on purpose. Upstream's reason map is merge-extensible, so a backend-added
/// variant is unnamable here; an unnameable ending over consumed input must not read as success.
#[must_use]
pub fn accounts_for_claim(reason: &TurnEndReason) -> bool {
    !matches!(reason, TurnEndReason::Completed)
}

/// Fold one agent log, or an owned suffix of one, into its account of consumed work.
///
/// Single pass, and every input is the log itself: no caller has to sample live state before
/// cancelling, so a cancellation issued by anyone — the owner's teardown, an ancestor's interrupt,
/// an unloading plugin — reads the same.
///
/// The rule, in one sentence: a turn accounts for work when it *stepped*, or when it *claimed* and
/// then ended in a way that accounts for the claim; a later cancellation of unrun work clears the
/// account, because the work it took is no longer what that turn finished.
#[must_use]
pub fn fold_consumed_work(events: &[Event<'_>]) -> ConsumedWork {
    // Turns that entered a step. A turn that reached a model call did run its work, whatever the
    // inbox says, so this set answers without needing the ending at all.
    let mut stepped: Vec<u64> = Vec::new();
    // Turns that claimed input while open. Whether the claim is accounted for depends on the
    // ending, which is why this is tracked separately from `stepped`.
    let mut claimed: Vec<u64> = Vec::new();
    let mut open: Option<u64> = None;
    let mut end = None;
    let mut dropped_unrun = false;

    for event in events {
        match *event {
            Event::TurnStart { turn } => open = Some(turn),

            Event::StepStart { turn } => {
                if !stepped.contains(&turn) {
                    stepped.push(turn);
                }
            }

            Event::InboxSpliced {
                removed,
                outcome,
                inserted,
            } => {
                // An insertion-only splice is not a claim: nothing left the queue.
                let Some(removed) = removed else { continue };
                if removed == 0 {
                    continue;
                }
                match outcome {
                    // A replacement keeps the work pending under a new identity, so only a
                    // cancellation that leaves nothing behind drops it.
                    SpliceOutcome::Canceled => {
                        if inserted == 0 {
                            dropped_unrun = true;
                        }
                    }
                    // Claims are the loop's own step-boundary reads, always inside a turn. A
                    // splice outside any open turn cannot claim work, so it is not counted.
                    SpliceOutcome::Consumed => {
                        if let Some(turn) = open {
                            if !claimed.contains(&turn) {
                                claimed.push(turn);
                            }
                        }
                    }
                }
            }

            Event::TurnEnd { turn, reason } => {
                open = None;
                let did_step = take(&mut stepped, turn);
                let did_claim = take(&mut claimed, turn);
                // "Claimed, then ended in a way that accounts for the claim" — a `completed` end
                // over an unstepped claim is the rewritten-away case, and is not an account.
                if did_step || (did_claim && accounts_for_claim(reason)) {
                    end = Some(EndedTurn {
                        turn,
                        reason: reason.clone(),
                    });
                    // Anything dropped before this turn closed is what its own ending reports;
                    // only a later drop is still unaccounted for.
                    dropped_unrun = false;
                }
            }
        }
    }

    ConsumedWork { end, dropped_unrun }
}

/// Removes `turn` from `set`, reporting whether it was there.
///
/// A `Vec` rather than a set because these hold a handful of turn numbers at most and the log is
/// walked once; the linear scan beats hashing at that size and keeps the order readable.
fn take(set: &mut Vec<u64>, turn: u64) -> bool {
    match set.iter().position(|candidate| *candidate == turn) {
        Some(position) => {
            set.remove(position);
            true
        }
        None => false,
    }
}
