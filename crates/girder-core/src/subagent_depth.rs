//! Delegation-depth accounting: the recursion budget a parent agent passes to its children.
//!
//! This is the Rust counterpart of upstream `packages/subagent/subagent/src/depth.ts`. Upstream keeps
//! the accounting in its own module so composition helpers can read it without importing the
//! subagent service; the same reason applies here, and it also lets the arithmetic be tested without
//! a runtime.
//!
//! # Why depth is monotone
//!
//! A persisted session header carries a `delegationDepth`, and a running agent also carries a depth
//! in its runtime options. Those two sources can disagree, because a **resumed** child is rebuilt
//! from its stored transcript and arrives with *fresh* runtime options — options that begin at the
//! top-level default of zero, since nothing in the resume path knows it used to be a child.
//!
//! Counting from the runtime value alone would therefore make a resumed child believe it is
//! top-level again. It would regain a full recursion budget on every resume, and a model that
//! delegates, is resumed, and delegates again would recurse until something outside this module
//! stopped it. So the depth is resolved as `max(persisted, runtime)`, and the module offers no way
//! to lower an existing [`Depth`]: every combination operator ([`Depth::deepest`],
//! [`Depth::resolve`]) returns the deeper input. "Restore" and "resume" may only ever confirm a
//! depth, never shrink it.
//!
//! # Why a refusal is an error and not a clamp
//!
//! A depth that saturates at the ceiling is indistinguishable from a depth that legitimately reached
//! it, so a child spawned after saturation would report the same identity as an existing sibling and
//! the operator would see a truncated tree with no signal that it was truncated. [`Depth::child`] and
//! [`Limit::child`] return [`DepthError`] instead, so the caller can refuse the delegation and say
//! why.

use thiserror::Error;

/// How far an agent sits from the root of its delegation tree.
///
/// [`Depth::TOP`] is the root agent (depth 0); each delegation produces a child at `parent + 1`.
/// The type is deliberately opaque about construction: a raw value can be decoded from a persisted
/// header with [`Depth::from_raw`], but the only arithmetic offered moves *up*.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Depth(u32);

impl Depth {
    /// The top-level agent's depth.
    ///
    /// A top-level agent is the one a user started directly, not one produced by delegation.
    pub const TOP: Self = Self(0);

    /// The depth as an integer.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Decodes a depth that was persisted in a session header.
    ///
    /// Header values were validated where the session was created, so they need no re-validation
    /// here, but they are *evidence about the past*, not a budget: feed them through
    /// [`Depth::resolve`] together with the runtime depth rather than adopting them directly, so a
    /// stale header cannot lower the count either.
    #[must_use]
    pub const fn from_raw(value: u32) -> Self {
        Self(value)
    }

    /// The depth of a child this agent delegates.
    ///
    /// Returns [`DepthError::Overflow`] at [`u32::MAX`] instead of saturating, because a saturated
    /// child would carry its parent's depth and look like a sibling; a silent clamp hides exactly the
    /// runaway recursion the ceiling exists to expose.
    pub fn child(self) -> Result<Self, DepthError> {
        let next = self
            .0
            .checked_add(1)
            .ok_or(DepthError::Overflow { depth: self.0 })?;
        Ok(Self(next))
    }

    /// The deeper of two observations of the same agent.
    ///
    /// This is the only way two depths combine. It is commutative, associative and idempotent, which
    /// is what makes "a resumed child may deepen but never shallow" hold no matter which source the
    /// caller read first.
    #[must_use]
    pub const fn deepest(self, other: Self) -> Self {
        if other.0 > self.0 { other } else { self }
    }

    /// Resolves an agent's depth from the two places it is recorded.
    ///
    /// `persisted` is the depth stored in the session header (absent for a session that predates the
    /// field, which is a top-level session); `runtime` is the depth in the current options (absent
    /// when the caller did not set one). The result is the maximum of the two, defaulting each to
    /// [`Depth::TOP`], matching upstream's `max(header, runtime)`. A resumed child with fresh options
    /// of zero therefore keeps its header depth instead of restarting at the top of the tree.
    #[must_use]
    pub fn resolve(persisted: Option<Self>, runtime: Option<Self>) -> Self {
        let persisted = persisted.map_or(Self::TOP, |depth| depth);
        let runtime = runtime.map_or(Self::TOP, |depth| depth);
        persisted.deepest(runtime)
    }
}

/// A ceiling on how deep delegation may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Limit {
    /// The maximum depth a delegated agent may reach.
    ///
    /// `max = 3` admits descendants at depths 1, 2 and 3; no agent at depth 3 may delegate again.
    pub max: u32,
}

impl Limit {
    /// A ceiling of `max`.
    #[must_use]
    pub const fn new(max: u32) -> Self {
        Self { max }
    }

    /// Whether an agent at `depth` may delegate one more level.
    ///
    /// The comparison is strictly `depth < max`, so the ceiling is the deepest *reachable* depth and
    /// is itself terminal: with `max = 3`, depths 0, 1 and 2 may delegate (producing children at 1, 2
    /// and 3), while a depth-3 agent may not. Reading the bound as `depth <= max` instead would admit
    /// a fourth level and make the operator's number off by one.
    #[must_use]
    pub const fn allows(self, depth: Depth) -> bool {
        depth.0 < self.max
    }

    /// Derives the child depth of a delegation by `parent`, or refuses it.
    ///
    /// Combines the ceiling check and the increment so a caller cannot perform one without the other
    /// and then have to remember which came first. The check runs first: a refused delegation spends
    /// nothing, and the error names the parent's depth so the message can be shown to the model.
    pub fn child(self, parent: Depth) -> Result<Depth, DepthError> {
        if !self.allows(parent) {
            return Err(DepthError::Exceeded {
                depth: parent.get(),
                max: self.max,
            });
        }
        parent.child()
    }
}

/// Why a depth could not be derived.
///
/// Both variants are refusals, never adjustments: a caller that receives one must not delegate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Error)]
pub enum DepthError {
    /// The configured ceiling forbids one more level.
    #[error("delegation refused: depth {depth} has reached the limit of {max}")]
    Exceeded {
        /// The depth of the agent that tried to delegate.
        depth: u32,
        /// The configured ceiling.
        max: u32,
    },

    /// The depth cannot be incremented because the counter is already at its maximum.
    ///
    /// Unreachable through a [`Limit`] below [`u32::MAX`], because [`Limit::allows`] refuses such a
    /// parent first; it exists so [`Depth::child`] can refuse instead of wrapping or saturating when
    /// called on its own.
    #[error("delegation refused: depth {depth} cannot be incremented further")]
    Overflow {
        /// The depth that could not be incremented.
        depth: u32,
    },
}
