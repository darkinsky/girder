//! When to replace a tool result with a spill locator.
//!
//! The store saves text; this decides whether to. The two are separate because the decision is policy — a shell
//! tool and a web fetch have different thresholds — while the storage is not.
//!
//! # Why the decision is a pure function
//!
//! A tool that asked "should I spill?" and got an answer it could not inspect would have to trust the policy. A
//! function over the text and its context returns the decision and the reason, so a caller can log why a result was
//! replaced and a test can check the boundary without a store.
//!
//! # Why the threshold counts bytes rather than characters
//!
//! A context window is measured in tokens, and tokens track bytes more closely than characters do for the content
//! tools produce. Counting characters would let a page of CJK text through at the same "size" as a much smaller
//! ASCII page.
//!
//! # Why a truncated result is still replaced
//!
//! A tool that already truncated its output to fit has produced something the model can read. Spilling it would
//! replace a readable answer with a pointer to the same text — the model would spend a turn reading what it had.
//! So the decision distinguishes "large" from "truncated", and only the first spills.

use crate::retention::Omitted;

/// What a tool produced, in the terms the decision needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    /// How many bytes the result carries.
    pub bytes: usize,
    /// Whether the tool already bounded its own output.
    ///
    /// A truncated result is readable as it stands; spilling it would point at the same text the model already has.
    pub truncated: bool,
}

/// What to do with a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Return it as it is.
    Keep,
    /// Save it and return a locator.
    Spill,
}

impl Action {
    /// Whether the result should be replaced.
    #[must_use]
    pub fn should_spill(self) -> bool {
        matches!(self, Self::Spill)
    }
}

/// Why a decision was made.
///
/// Returned beside the action so a caller can log it: a result that was replaced without a recorded reason is a
/// behaviour nobody can explain after the fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Under the threshold.
    Small,
    /// The tool already bounded it, so it is readable as it stands.
    AlreadyBounded,
    /// Over the threshold and not bounded.
    TooLarge {
        /// How many bytes.
        bytes: usize,
        /// The threshold.
        limit: usize,
    },
}

impl Reason {
    /// A one-line explanation, for a log.
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Self::Small => "under the spill threshold".to_owned(),
            Self::AlreadyBounded => "the tool already bounded its output".to_owned(),
            Self::TooLarge { bytes, limit } => {
                format!("{bytes} bytes exceeds the {limit}-byte spill threshold")
            }
        }
    }
}

/// Decides what to do with one result.
#[must_use]
pub fn decide(context: Context, limit: usize) -> (Action, Reason) {
    if context.truncated {
        return (Action::Keep, Reason::AlreadyBounded);
    }
    if context.bytes > limit {
        return (
            Action::Spill,
            Reason::TooLarge {
                bytes: context.bytes,
                limit,
            },
        );
    }
    (Action::Keep, Reason::Small)
}

/// The omitted count a spill records.
///
/// A spill carries everything, so nothing is omitted: the model is told where the text is, not that some of it was
/// dropped. Returning a count here would claim the spill lost something, and a model that believed that would not
/// bother reading the file.
#[must_use]
pub fn omitted() -> Omitted {
    Omitted::None
}
