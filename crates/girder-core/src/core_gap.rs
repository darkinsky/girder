//! Session-log algebra: the model-visible surface, pending-call repair, and seq-range framing.
//!
//! # Where this comes from
//!
//! Upstream keeps this logic in `packages/core/session/src/`:
//!
//! - `surface.ts` — `SurfaceOp`, `isSurfaceEligibleType`, `isAppendSurfaceEvent`,
//!   `isReplacementSurfaceEvent`, `deriveEventMessage`, `validateSurfaceMetadata`, `foldSurface`.
//! - `repair.ts` — `TOOL_NOT_STARTED`, `TOOL_OUTCOME_UNKNOWN`, `ToolCallRecovery`,
//!   `openTurnClosers`, `interruptedTurnClosers`.
//! - `seq-ranges.ts` — `encodeSeqRanges`, `decodeSeqRanges`.
//!
//! # Why this gap, and why here
//!
//! [`girder_core::SessionStore`]'s module names this gap itself: "Upstream's `request-header`, `seq-ranges`, `repair`,
//! `surface`, and `fork` modules exist because a real session is resumed, compacted, and branched.
//! The slice needs append and replay only." That was true of the slice; it is not true of a session
//! that is compacted, forked, or recovered after a crash, and the three modules above are what those
//! operations are built from.
//!
//! The gap is *not* "we have no compaction" — the `compaction` and `log_compact` modules
//! exist. The gap is that local events carry neither surface placement nor provenance, so a
//! compaction cannot express *what it replaced*. Upstream's answer is a surface operation on each
//! message-producing event plus a fold that materialises the current ordered view. That is what this
//! module adds.
//!
//! # The model, in one paragraph
//!
//! The append-only log is the source of truth; the *surface* is an ordered list of log positions
//! that produce model-visible messages. Most events append themselves to the tail. A compaction
//! instead appends an event whose op is [`SurfaceOp::Replace`]: it shadows the nodes in that range
//! and stands where they stood. "Shadowed" rather than "deleted" because the original bytes remain
//! in the log — that is what lets a human transcript be reconstructed from append-origin events
//! while the model sees the compacted surface. Two readers, one log.
//!
//! # Why a parallel envelope instead of new event variants
//!
//! [`SessionEvent`] is a closed, tagged enum whose variants carry conversation payload. Adding
//! placement to the variants would give every variant (and every future one) the same two optional
//! fields, and would make an invalid combination — a `turn/start` carrying a surface op —
//! representable. Upstream enforces the constraint with conditional mapped types on
//! `SessionEvent<T>`; Rust gets it for free by keeping placement in a parallel structure keyed by
//! log position. [`SurfaceEnvelope`] pairs an event with its placement, so the payload type is
//! untouched and placement cannot migrate onto a boundary event. [`SurfaceEnvelope::new`] rejects
//! that combination at construction, and [`fold_surface`] re-checks it so a struct literal cannot
//! bypass the rule.
//!
//! # What this module deliberately does not do
//!
//! - No I/O and no [`girder_core::SessionStore`] ownership. Placement is supplied by the caller
//!   when appending, and the fold is a pure function over a slice. That is what makes every rule
//!   below testable without a temporary file.
//! - No `request/header` fold and no `ToolHistoryProjection`. Those need a tool-schema vocabulary
//!   local events do not carry yet; inventing one here would create a second source of truth for
//!   tool declarations.
//! - No plugin message projections (`MESSAGE_PROJECTION_EVENT_TYPES`). Upstream throws when such an
//!   event appears without its interpreter; so does this module, rather than folding a message it
//!   cannot interpret.

use std::collections::BTreeSet;

// Imports go through the crate root's public re-exports rather than `crate::module` paths because
// this file is also compiled by `tests/core_gap.rs` via `#[path]`. In that build the file is a module
// of the *test* crate, where `crate::` names the test crate and every `crate::…` path fails to
// resolve. The root re-exports work identically in both builds, which is what keeps one source file
// the single thing under test.
use girder_core::{ChatMessage as Message, ContentBlock, SessionEvent, ToolCallId, TurnEndReason};

/// The recovery code for an assistant tool request that never reached a recorded call start.
///
/// Upstream's `TOOL_NOT_STARTED`. Distinct from [`TOOL_OUTCOME_UNKNOWN`] because the two demand
/// opposite advice: a call that provably never started is safe to retry, while one that may have
/// run is not.
pub const TOOL_NOT_STARTED: &str = "TOOL_NOT_STARTED";

/// The recovery code for a tool call whose completed outcome was not durably recorded.
///
/// Upstream's `TOOL_OUTCOME_UNKNOWN`. The call started but no result landed, so the side effect may
/// or may not have happened.
pub const TOOL_OUTCOME_UNKNOWN: &str = "TOOL_OUTCOME_UNKNOWN";

/// The event types that can occupy the model-visible surface.
///
/// Upstream's `SURFACE_EVENT_TYPES`. Membership is what makes a placement legal: only these five may
/// carry a [`SurfaceOp`], and every one of them must.
const SURFACE_EVENT_TYPES: &[&str] = &[
    "system/message",
    "developer/message",
    "user/message",
    "assistant/message",
    "tool/result",
];

/// Event types whose model-visible effect requires an explicit interpreter.
///
/// Upstream's `MESSAGE_PROJECTION_EVENT_TYPES`. This crate has no projection registry, so the honest
/// behaviour is to refuse the log rather than fold a history that is known to be wrong.
const MESSAGE_PROJECTION_EVENT_TYPES: &[&str] = &["image/offload"];

/// How an event entered the ordered surface.
///
/// Upstream's `SurfaceOp`: either the string `'append'` or `{ op: 'replace', startSeq, endSeq }`.
/// Modelled as an enum so the two shapes cannot be confused, and so the invalid object shapes
/// upstream rejects at runtime (`{ op: 'append', startSeq: 3 }`) are not representable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceOp {
    /// The event joined the tail — the normal path for prompts, replies, and tool results.
    Append,
    /// The event shadows the inclusive surface range `start..=end` and stands where it stood.
    ///
    /// The range is expressed through the *sequences of current surface nodes*, not as a numeric
    /// interval, because a surface with replacements is not numerically contiguous.
    Replace {
        /// Sequence of the first surface node to shadow.
        start: u64,
        /// Sequence of the last surface node to shadow, inclusive.
        end: u64,
    },
}

impl SurfaceOp {
    /// Whether this op is an append.
    #[must_use]
    pub const fn is_append(self) -> bool {
        matches!(self, Self::Append)
    }

    /// Whether this op shadows an existing range.
    #[must_use]
    pub const fn is_replace(self) -> bool {
        matches!(self, Self::Replace { .. })
    }
}

/// One log event together with where it sits on the model-visible surface.
///
/// Upstream stores placement *on* the event (`surfaceOp`, `sourceEventSeqs`). The split here is
/// deliberate; see the module docs on why a closed enum benefits from a parallel envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceEnvelope {
    /// The durable log event. Its payload is untouched.
    pub event: SessionEvent,
    /// Position in the log; the key every placement and provenance reference uses.
    pub seq: u64,
    /// How the event entered the surface: `None` for a log-only event.
    ///
    /// `None` on a surface-eligible type is rejected by the fold; `Some` on any other type is
    /// rejected by [`SurfaceEnvelope::new`].
    pub op: Option<SurfaceOp>,
    /// Earlier log positions this event was derived from — upstream's `sourceEventSeqs`.
    ///
    /// A replacement must cite every node it shadows, which is what makes provenance checkable
    /// rather than merely asserted.
    pub sources: Vec<u64>,
}

impl SurfaceEnvelope {
    /// Pairs an event with its placement, rejecting the combinations the log may not contain.
    ///
    /// # Errors
    ///
    /// Returns [`SurfaceError::NotSurfaceEligible`] when a non-surface event carries a placement or
    /// provenance. This is upstream's rule in `surfaceOpOf`, moved to construction so a writer
    /// cannot forget it.
    pub fn new(
        event: SessionEvent,
        seq: u64,
        op: Option<SurfaceOp>,
        sources: Vec<u64>,
    ) -> Result<Self, SurfaceError> {
        let eligible = is_surface_eligible_type(event.type_name());
        if !eligible && op.is_some() {
            return Err(SurfaceError::NotSurfaceEligible {
                detail: format!(
                    "event \"{}\" at seq {seq} is not surface-eligible and cannot carry a surface op",
                    event.type_name()
                ),
            });
        }
        if !eligible && !sources.is_empty() {
            return Err(SurfaceError::NotSurfaceEligible {
                detail: format!(
                    "event \"{}\" at seq {seq} is not surface-eligible and cannot carry source references",
                    event.type_name()
                ),
            });
        }
        Ok(Self {
            event,
            seq,
            op,
            sources,
        })
    }

    /// An event that appends to the surface tail.
    #[must_use]
    pub fn appended(event: SessionEvent, seq: u64) -> Self {
        Self {
            event,
            seq,
            op: Some(SurfaceOp::Append),
            sources: Vec::new(),
        }
    }

    /// A log-only event: boundaries, accounting, and settings records.
    #[must_use]
    pub fn log_only(event: SessionEvent, seq: u64) -> Self {
        Self {
            event,
            seq,
            op: None,
            sources: Vec::new(),
        }
    }

    /// An event that shadows the surface range `start..=end`.
    #[must_use]
    pub fn replacing(
        event: SessionEvent,
        seq: u64,
        start: u64,
        end: u64,
        sources: Vec<u64>,
    ) -> Self {
        Self {
            event,
            seq,
            op: Some(SurfaceOp::Replace { start, end }),
            sources,
        }
    }

    /// The event's `type` tag.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.event.type_name()
    }
}

/// Whether an event type can join the model-visible surface.
///
/// Upstream's `isSurfaceEligibleType`.
#[must_use]
pub fn is_surface_eligible_type(type_name: &str) -> bool {
    SURFACE_EVENT_TYPES.contains(&type_name)
}

/// Whether an event type needs an interpreter before it can be folded.
///
/// Upstream's `MESSAGE_PROJECTION_EVENT_TYPES`. Exposed so a caller can check a log for needed
/// plugins before attempting a fold.
#[must_use]
pub fn needs_message_projection(type_name: &str) -> bool {
    MESSAGE_PROJECTION_EVENT_TYPES.contains(&type_name)
}

/// Whether an envelope joined the surface at its own log position.
///
/// Upstream's `isAppendSurfaceEvent`. This is the human-transcript source: a replacement would erase
/// conversation a reader already saw, so a transcript reads append-origin events while a model reads
/// the fold.
#[must_use]
pub fn is_append_surface_event(envelope: &SurfaceEnvelope) -> bool {
    envelope.op == Some(SurfaceOp::Append)
}

/// Whether an envelope shadowed an existing surface range.
///
/// Upstream's `isReplacementSurfaceEvent`.
#[must_use]
pub fn is_replacement_surface_event(envelope: &SurfaceEnvelope) -> bool {
    matches!(envelope.op, Some(SurfaceOp::Replace { .. }))
}

/// The message an envelope contributes to model input, or `None` when it contributes none.
///
/// Upstream's `deriveEventMessage`, including both of its non-obvious cases:
///
/// - A `user/message` is returned even when empty, because an empty prompt is still a turn the user
///   took and dropping it would make the transcript disagree with the log.
/// - System, developer, and assistant messages with no content return `None` rather than an empty
///   message: an empty assistant event exists to host a max-tokens step's usage accounting, and
///   injecting it as a content-less turn violates the provider protocol.
///
/// Tool results are absent here by design, not omission: local [`SessionEvent::ToolResult`] carries
/// blocks inline instead of wrapping a [`Message`], so there is no message to borrow. The fold still
/// places the node and [`tool_result_content`] reads its content.
#[must_use]
pub fn derive_event_message(envelope: &SurfaceEnvelope) -> Option<&Message> {
    match &envelope.event {
        SessionEvent::UserMessage { message } => Some(message),
        SessionEvent::SystemMessage { message, .. }
        | SessionEvent::DeveloperMessage { message, .. }
        | SessionEvent::AssistantMessage { message } => {
            if message.content.is_empty() {
                None
            } else {
                Some(message)
            }
        }
        _ => None,
    }
}

/// The content blocks a tool-result envelope carries.
///
/// The counterpart of [`derive_event_message`] for the one surface type whose local shape has no
/// [`Message`] wrapper, so a compactor can read what it is replacing.
#[must_use]
pub fn tool_result_content(envelope: &SurfaceEnvelope) -> Option<&[ContentBlock]> {
    match &envelope.event {
        SessionEvent::ToolResult { content, .. } => Some(content),
        _ => None,
    }
}

/// The outcome of folding a log through the surface rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceFold {
    /// Current surface node sequences, in model-visible order.
    nodes: Vec<u64>,
    /// Every replacement that occurred, in log order.
    replacements: Vec<SurfaceReplacement>,
}

impl SurfaceFold {
    /// The current surface node sequences, in model-visible order.
    #[must_use]
    pub fn nodes(&self) -> &[u64] {
        &self.nodes
    }

    /// Every replacement, in log order.
    ///
    /// Retained because a compaction report and a "what did this replace" view both need the
    /// history, not just the final state.
    #[must_use]
    pub fn replacements(&self) -> &[SurfaceReplacement] {
        &self.replacements
    }

    /// Whether `seq` survived to the current surface.
    ///
    /// A shadowed node is absent from [`Self::nodes`]; this is the predicate form of "did a
    /// compaction cover this message".
    #[must_use]
    pub fn is_visible(&self, seq: u64) -> bool {
        self.nodes.contains(&seq)
    }

    /// The number of nodes currently on the surface.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the surface is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// One replace transition, as it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceReplacement {
    /// Sequence of the replacing event.
    pub seq: u64,
    /// First shadowed node.
    pub start: u64,
    /// Last shadowed node, inclusive.
    pub end: u64,
    /// Every node the replacement shadowed, in surface order.
    pub shadowed: Vec<u64>,
}

/// Why a placement is illegal or a log cannot be folded.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SurfaceError {
    /// A non-surface event carried placement or provenance.
    #[error("surface: {detail}")]
    NotSurfaceEligible {
        /// What went wrong, naming the event and sequence.
        detail: String,
    },

    /// A surface-eligible event carried no placement.
    #[error(
        "surface: event \"{type_name}\" at seq {seq} is surface-eligible and requires a surface op"
    )]
    MissingSurfaceOp {
        /// The event type.
        type_name: &'static str,
        /// Its sequence.
        seq: u64,
    },

    /// Log sequences were not contiguous from zero.
    ///
    /// Contiguity is required because a surface position doubles as an index into the log for
    /// provenance checks; a gap would silently validate against the wrong event.
    #[error("surface: event seq {actual} is not contiguous; expected {expected}")]
    NotContiguous {
        /// The sequence found.
        actual: u64,
        /// The sequence expected there.
        expected: u64,
    },

    /// A replacement named a node the surface does not hold, or named it out of order.
    #[error("surface: replace at seq {seq}: {detail}")]
    InvalidRange {
        /// The replacing event.
        seq: u64,
        /// What is wrong with the range.
        detail: String,
    },

    /// A replacement tried to reference the future or itself.
    #[error(
        "surface: replace at seq {seq}: range must reference earlier events, but start {start} or end {end} is not earlier"
    )]
    RangeNotEarlier {
        /// The replacing event.
        seq: u64,
        /// The requested range start.
        start: u64,
        /// The requested range end.
        end: u64,
    },

    /// Provenance was empty, duplicated, or in the future.
    #[error("surface: event at seq {seq} has invalid provenance: {detail}")]
    InvalidProvenance {
        /// The offending event.
        seq: u64,
        /// What is wrong with it.
        detail: String,
    },

    /// A replacement omitted one of the nodes it shadows.
    #[error(
        "surface: replace at seq {seq}: sources must include every shadowed node; missing {missing:?}"
    )]
    UncitedShadow {
        /// The replacing event.
        seq: u64,
        /// Shadowed nodes absent from its provenance.
        missing: Vec<u64>,
    },

    /// A `tool/result` replacement changed something other than content.
    ///
    /// A tool result may be pruned to save context, but its identity, call, and error flag are facts
    /// about what happened; rewriting them would make the log lie about the run.
    #[error("surface: replace at seq {seq}: a tool/result replacement may change only its content")]
    ToolResultRewriteChangedFacts {
        /// The replacing event.
        seq: u64,
    },

    /// Node zero holds the system prompt, which only a system message may rewrite.
    #[error(
        "surface: replace at seq {seq}: node {start} holds the system prompt and may be rewritten only by a system/message over exactly that node"
    )]
    SystemHeadProtected {
        /// The replacing event.
        seq: u64,
        /// The node at position zero.
        start: u64,
    },

    /// The log contains an event type needing an interpreter this crate does not have.
    #[error(
        "surface: event \"{type_name}\" at seq {seq} requires a message projection; load its owning plugin or drop the record"
    )]
    MissingProjection {
        /// The event type.
        type_name: &'static str,
        /// Its sequence.
        seq: u64,
    },
}

/// Validates one envelope's placement against the rules that do not need the surface.
///
/// Upstream's `validateSurfaceMetadata` plus the structural half of `assertSourceEventReferences`.
/// Public because a writer appending live events needs the same check a replay does; [`fold_surface`]
/// calls it too, so there is one definition rather than two that drift.
///
/// # Errors
///
/// Reports [`SurfaceError`] for a surface-eligible type with no op, a placement on a non-surface
/// type, a replace range that does not point strictly backwards, empty or duplicated or future
/// provenance, and an assistant message that cites sources despite embedding its own stream.
pub fn validate_surface_metadata(envelope: &SurfaceEnvelope) -> Result<(), SurfaceError> {
    let eligible = is_surface_eligible_type(envelope.type_name());
    match envelope.op {
        None => {
            if eligible {
                return Err(SurfaceError::MissingSurfaceOp {
                    type_name: envelope.type_name(),
                    seq: envelope.seq,
                });
            }
            return Ok(());
        }
        Some(op) => {
            if !eligible {
                return Err(SurfaceError::NotSurfaceEligible {
                    detail: format!(
                        "event \"{}\" at seq {} is not surface-eligible and cannot carry a surface op",
                        envelope.type_name(),
                        envelope.seq
                    ),
                });
            }
            if let SurfaceOp::Replace { start, end } = op
                && (start >= envelope.seq || end >= envelope.seq)
            {
                return Err(SurfaceError::RangeNotEarlier {
                    seq: envelope.seq,
                    start,
                    end,
                });
            }
        }
    }

    validate_provenance(envelope)?;

    if envelope.type_name() == "assistant/message" && !envelope.sources.is_empty() {
        // An assistant message embeds the provider stream it came from; citing earlier events too
        // would give a reader two competing accounts of what produced it.
        return Err(SurfaceError::InvalidProvenance {
            seq: envelope.seq,
            detail: "assistant/message embeds its source stream and cannot cite earlier events"
                .to_owned(),
        });
    }

    Ok(())
}

/// Checks the structural provenance rules that do not depend on the surface.
fn validate_provenance(envelope: &SurfaceEnvelope) -> Result<(), SurfaceError> {
    let mut seen = BTreeSet::new();
    for &source in &envelope.sources {
        if source >= envelope.seq {
            return Err(SurfaceError::InvalidProvenance {
                seq: envelope.seq,
                detail: format!(
                    "sources must reference earlier events; {source} >= current seq {}",
                    envelope.seq
                ),
            });
        }
        if !seen.insert(source) {
            return Err(SurfaceError::InvalidProvenance {
                seq: envelope.seq,
                detail: format!("sources must not contain duplicates; {source} repeats"),
            });
        }
    }
    Ok(())
}

/// Locates a replacement range on the surface without mutating the fold.
///
/// Returns `(start_index, end_index, shadowed_sequences)`.
fn locate_range(
    nodes: &[u64],
    seq: u64,
    start: u64,
    end: u64,
) -> Result<(usize, usize, Vec<u64>), SurfaceError> {
    let start_index = nodes
        .iter()
        .position(|node| *node == start)
        .ok_or_else(|| SurfaceError::InvalidRange {
            seq,
            detail: format!("start seq {start} is not on the current surface"),
        })?;
    let end_index =
        nodes
            .iter()
            .position(|node| *node == end)
            .ok_or_else(|| SurfaceError::InvalidRange {
                seq,
                detail: format!("end seq {end} is not on the current surface"),
            })?;
    if start_index > end_index {
        return Err(SurfaceError::InvalidRange {
            seq,
            detail: format!(
                "start seq {start} (position {start_index}) is after end seq {end} (position {end_index})"
            ),
        });
    }
    Ok((
        start_index,
        end_index,
        nodes[start_index..=end_index].to_vec(),
    ))
}

/// Replays a whole log through the surface rules.
///
/// Upstream's `foldSurface`. Returns the ordered surface and every replacement that happened.
///
/// # Errors
///
/// Rejects a log whose sequences are not contiguous from zero, a surface-eligible event with no
/// placement, a replacement range not on the current surface, provenance that fails to cite every
/// shadowed node, a `tool/result` replacement that changes facts, a replacement of the system prompt
/// by anything but a system message, and an event needing an interpreter this crate lacks.
pub fn fold_surface(log: &[SurfaceEnvelope]) -> Result<SurfaceFold, SurfaceError> {
    let mut nodes: Vec<u64> = Vec::with_capacity(log.len());
    let mut replacements = Vec::new();

    for (index, envelope) in log.iter().enumerate() {
        let expected = u64::try_from(index).unwrap_or(u64::MAX);
        if envelope.seq != expected {
            return Err(SurfaceError::NotContiguous {
                actual: envelope.seq,
                expected,
            });
        }
        if needs_message_projection(envelope.type_name()) {
            return Err(SurfaceError::MissingProjection {
                type_name: envelope.type_name(),
                seq: envelope.seq,
            });
        }
        validate_surface_metadata(envelope)?;

        match envelope.op {
            None => {}
            Some(SurfaceOp::Append) => nodes.push(envelope.seq),
            Some(SurfaceOp::Replace { start, end }) => {
                let (start_index, end_index, shadowed) =
                    locate_range(&nodes, envelope.seq, start, end)?;

                let missing: Vec<u64> = shadowed
                    .iter()
                    .copied()
                    .filter(|node| !envelope.sources.contains(node))
                    .collect();
                if !missing.is_empty() {
                    // Provenance is what makes "these were replaced by this" auditable. A
                    // replacement that silently drops a node hides the fact it existed.
                    return Err(SurfaceError::UncitedShadow {
                        seq: envelope.seq,
                        missing,
                    });
                }

                check_tool_result_rewrite(log, envelope, &shadowed)?;
                let head = nodes.first().copied();
                check_system_head(log, head, start_index, envelope, &shadowed)?;

                // The range collapses to the replacing node, which keeps the range's position
                // rather than moving to the tail: a compaction summarizes where it happened.
                nodes.splice(start_index..=end_index, [envelope.seq]);
                replacements.push(SurfaceReplacement {
                    seq: envelope.seq,
                    start,
                    end,
                    shadowed,
                });
            }
        }
    }

    Ok(SurfaceFold {
        nodes,
        replacements,
    })
}

/// Restricts a `tool/result` replacement to changing content only.
fn check_tool_result_rewrite(
    log: &[SurfaceEnvelope],
    envelope: &SurfaceEnvelope,
    shadowed: &[u64],
) -> Result<(), SurfaceError> {
    let SessionEvent::ToolResult {
        id,
        is_error: replacement_is_error,
        ..
    } = &envelope.event
    else {
        return Ok(());
    };
    if shadowed.len() != 1 {
        return Err(SurfaceError::InvalidRange {
            seq: envelope.seq,
            detail: "a tool/result replacement must rewrite exactly one current node".to_owned(),
        });
    }
    let original = shadowed
        .first()
        .and_then(|seq| log.iter().find(|candidate| candidate.seq == *seq))
        .map(|candidate| &candidate.event);
    let Some(SessionEvent::ToolResult {
        id: original_id,
        is_error: original_is_error,
        ..
    }) = original
    else {
        return Err(SurfaceError::InvalidRange {
            seq: envelope.seq,
            detail: "a tool/result replacement must target a current tool/result".to_owned(),
        });
    };
    if original_id != id || original_is_error != replacement_is_error {
        return Err(SurfaceError::ToolResultRewriteChangedFacts { seq: envelope.seq });
    }
    Ok(())
}

/// Protects node zero when it holds the system prompt.
///
/// A compaction may shadow later system nodes — the prompt is re-assembled per step — but the first
/// one is the contract the model was given at the start of the session, and a summary standing in
/// for it would silently change the agent's rules.
fn check_system_head(
    log: &[SurfaceEnvelope],
    head: Option<u64>,
    start_index: usize,
    envelope: &SurfaceEnvelope,
    shadowed: &[u64],
) -> Result<(), SurfaceError> {
    if start_index != 0 {
        return Ok(());
    }
    let Some(head_seq) = head else {
        return Ok(());
    };
    let head_is_system = log
        .iter()
        .find(|candidate| candidate.seq == head_seq)
        .is_some_and(|candidate| matches!(candidate.event, SessionEvent::SystemMessage { .. }));
    if !head_is_system {
        return Ok(());
    }
    let rewritten_by_system =
        matches!(envelope.event, SessionEvent::SystemMessage { .. }) && shadowed.len() == 1;
    if rewritten_by_system {
        return Ok(());
    }
    Err(SurfaceError::SystemHeadProtected {
        seq: envelope.seq,
        start: head_seq,
    })
}

/// Why an open tail turn is being closed synthetically.
///
/// Upstream's `OpenTurnCloseCause`. The cause selects the wording of the synthetic tool results,
/// because a crashed session and a fork cut mid-turn are different facts and a reader should not
/// have to guess which happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenTurnCloseCause {
    /// Crash recovery over a persisted log.
    Interrupted,
    /// A fork seed cut inside the source session's open turn.
    Forked,
}

impl OpenTurnCloseCause {
    /// The `kind` spelling used in a synthetic `turn/end` reason, matching upstream's tag.
    #[must_use]
    pub const fn reason_kind(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::Forked => "forked",
        }
    }

    /// Model-visible wording for a call that never started.
    const fn not_started_text(self) -> &'static str {
        match self {
            Self::Interrupted => {
                "The tool call was interrupted before the Harness recorded it as started. Retry it if it is still needed."
            }
            Self::Forked => {
                "The history inherited by this branch has no record of this tool call starting. The parent session may have executed it after the fork point. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly."
            }
        }
    }

    /// Model-visible wording for a call whose outcome is unknown.
    const fn outcome_unknown_text(self) -> &'static str {
        match self {
            Self::Interrupted => {
                "The tool call was interrupted after it was recorded, but no result was durably recorded. Its outcome is unknown. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly."
            }
            Self::Forked => {
                "The history inherited by this branch records this tool call starting but does not include its result. The parent session may have completed it after the fork point. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly."
            }
        }
    }
}

/// One synthetic tool result to append, before it becomes an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntheticToolResult {
    /// The call this answers.
    pub call_id: String,
    /// `true` when the log recorded the call starting, making the outcome unknown rather than
    /// provably absent.
    pub started: bool,
    /// The recovery code: [`TOOL_NOT_STARTED`] or [`TOOL_OUTCOME_UNKNOWN`].
    pub code: &'static str,
    /// The model-visible explanation.
    pub text: String,
}

/// Tracks assistant tool requests that no result has answered.
///
/// Upstream's `ToolCallRecovery`. This is the state behind both crash recovery and fork seeding: a
/// log ending with an assistant message requesting tools and no results is not corrupt, it is
/// *open*, and a provider rejects the next request that carries a dangling call.
///
/// Unlike upstream — where a call's turn and step are read off the event — local
/// [`SessionEvent::AssistantMessage`] carries no turn or step, so the tracker records only the two
/// facts recovery needs: the call id, and whether a [`SessionEvent::ToolCall`] ever recorded it as
/// started.
#[derive(Debug)]
pub struct ToolCallRecovery {
    /// Pending calls in assistant order, each with its started flag.
    ///
    /// A `Vec` rather than a map because synthetic results must be emitted in the order the
    /// assistant requested them, which is the transcript order a provider expects.
    pending: Vec<(String, bool)>,
    /// The cause that selects synthetic wording.
    cause: OpenTurnCloseCause,
}

impl ToolCallRecovery {
    /// Starts tracking with a cause that selects the synthetic wording.
    #[must_use]
    pub fn new(cause: OpenTurnCloseCause) -> Self {
        Self {
            pending: Vec::new(),
            cause,
        }
    }

    /// The cause this tracker closes with.
    #[must_use]
    pub fn cause(&self) -> OpenTurnCloseCause {
        self.cause
    }

    /// Consumes the next event in log order.
    pub fn observe(&mut self, event: &SessionEvent) {
        match event {
            // A turn or step boundary discards pending calls: either they were answered inside the
            // step, or the step ended without answering them and the boundary is the record of that.
            SessionEvent::TurnStart { .. }
            | SessionEvent::TurnEnd { .. }
            | SessionEvent::StepEnd { .. } => self.pending.clear(),

            SessionEvent::AssistantMessage { message } => {
                for block in &message.content {
                    if let ContentBlock::ToolCall { id, .. } = block
                        && !self.pending.iter().any(|(known, _)| known == id.as_str())
                    {
                        self.pending.push((id.as_str().to_owned(), false));
                    }
                }
            }

            SessionEvent::ToolCall { id, .. } => {
                if let Some(entry) = self
                    .pending
                    .iter_mut()
                    .find(|(known, _)| known == id.as_str())
                {
                    entry.1 = true;
                }
            }

            SessionEvent::ToolResult { id, .. } => {
                self.pending.retain(|(known, _)| known != id.as_str());
            }

            _ => {}
        }
    }

    /// The call ids still awaiting a result, in assistant order.
    #[must_use]
    pub fn pending(&self) -> Vec<&str> {
        self.pending.iter().map(|(id, _)| id.as_str()).collect()
    }

    /// The pending calls with their started flags, in assistant order.
    #[must_use]
    pub fn pending_with_state(&self) -> Vec<(&str, bool)> {
        self.pending
            .iter()
            .map(|(id, started)| (id.as_str(), *started))
            .collect()
    }

    /// Whether any call is still awaiting a result.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Builds the synthetic results for the calls still pending.
    ///
    /// Upstream's `ToolCallRecovery.results`. Split from [`open_turn_closers`] so a caller that wants
    /// to report before it writes can do so.
    #[must_use]
    pub fn results(&self) -> Vec<SyntheticToolResult> {
        self.pending_with_state()
            .into_iter()
            .map(|(call_id, started)| SyntheticToolResult {
                call_id: call_id.to_owned(),
                started,
                code: if started {
                    TOOL_OUTCOME_UNKNOWN
                } else {
                    TOOL_NOT_STARTED
                },
                text: if started {
                    self.cause.outcome_unknown_text().to_owned()
                } else {
                    self.cause.not_started_text().to_owned()
                },
            })
            .collect()
    }
}

/// Builds the synthetic events that close an open tail turn.
///
/// Upstream's `openTurnClosers`. Returns, in append order: one error [`SessionEvent::ToolResult`]
/// per unanswered call, then [`SessionEvent::StepEnd`] when a step was open, then
/// [`SessionEvent::TurnEnd`] whose reason names the cause. A balanced log returns an empty vector —
/// recovery of a healthy session writes nothing.
///
/// A `turn/end` is emitted even when the log's last event already closed the step, because the turn
/// is what is open. Both synthetic events carry the real turn number so a reader attributes them to
/// the turn they close rather than to a turn zero that never existed.
#[must_use]
pub fn open_turn_closers(log: &[SurfaceEnvelope], cause: OpenTurnCloseCause) -> Vec<SessionEvent> {
    let mut recovery = ToolCallRecovery::new(cause);
    let mut open_turn: Option<u64> = None;
    let mut open_step: Option<u64> = None;

    for envelope in log {
        recovery.observe(&envelope.event);
        match &envelope.event {
            SessionEvent::TurnStart { turn } => {
                open_turn = Some(*turn);
                open_step = None;
            }
            SessionEvent::TurnEnd { .. } => {
                open_turn = None;
                open_step = None;
            }
            SessionEvent::StepStart { step, .. } => open_step = Some(*step),
            SessionEvent::StepEnd { .. } => open_step = None,
            _ => {}
        }
    }

    let Some(turn) = open_turn else {
        return Vec::new();
    };

    let mut closers = Vec::new();
    for (index, result) in recovery.results().into_iter().enumerate() {
        let Some(id) = brand_call_id(&result.call_id, index) else {
            // Unreachable: `result.call_id` came from a branded id already, and the fallback is
            // pure ASCII alphanumeric plus `-`. Skipping rather than panicking keeps this module
            // panic-free even on a hand-corrupted log.
            continue;
        };
        closers.push(SessionEvent::ToolResult {
            id,
            content: vec![ContentBlock::Text { text: result.text }],
            is_error: true,
        });
    }

    if let Some(step) = open_step {
        closers.push(SessionEvent::StepEnd { turn, step });
    }
    closers.push(SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Aborted {
            cause: cause.reason_kind().to_owned(),
        },
    });
    closers
}

/// Brands a call id, falling back to a positional placeholder when the recorded value is not legal.
///
/// The fallback exists only for a hand-edited log; every id that reached the log was branded on the
/// way in. Returning `None` instead of panicking keeps recovery total.
fn brand_call_id(value: &str, index: usize) -> Option<ToolCallId> {
    ToolCallId::new(value)
        .or_else(|_| ToolCallId::new(format!("recovered-call-{index}")))
        .ok()
}

/// Crash-recovery entry point: closers for a persisted log whose tail turn was interrupted.
///
/// Upstream's `interruptedTurnClosers`, kept as a named entry point because that is the call site
/// recovery code actually uses and the name documents intent there.
#[must_use]
pub fn interrupted_turn_closers(log: &[SurfaceEnvelope]) -> Vec<SessionEvent> {
    open_turn_closers(log, OpenTurnCloseCause::Interrupted)
}

/// One entry of a losslessly encoded sequence list.
///
/// Upstream's `EncodedSeq = number | [number, number]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodedSeq {
    /// A single sequence.
    Single(u64),
    /// An inclusive consecutive run of at least three sequences.
    Range {
        /// First sequence.
        start: u64,
        /// Last sequence, inclusive.
        end: u64,
    },
}

/// Encodes a sequence list, collapsing runs of three or more into inclusive ranges.
///
/// Upstream's `encodeSeqRanges`. The threshold is three, not two: a range costs the same two numbers
/// as the two it would replace, so a two-element run buys no saving. Input that is not strictly
/// increasing is returned as singles, because collapsing it would either lose the repeats or invent
/// an ordering the caller did not mean.
#[must_use]
pub fn encode_seq_ranges(values: &[u64]) -> Vec<EncodedSeq> {
    if !is_strictly_increasing(values) {
        return values.iter().copied().map(EncodedSeq::Single).collect();
    }
    let mut encoded = Vec::new();
    let mut start = 0;
    while start < values.len() {
        let mut end = start;
        while end + 1 < values.len() && values[end + 1] == values[end] + 1 {
            end += 1;
        }
        if end - start >= 2 {
            encoded.push(EncodedSeq::Range {
                start: values[start],
                end: values[end],
            });
        } else {
            for value in &values[start..=end] {
                encoded.push(EncodedSeq::Single(*value));
            }
        }
        start = end + 1;
    }
    encoded
}

/// Whether every element is greater than the one before it.
fn is_strictly_increasing(values: &[u64]) -> bool {
    values
        .windows(2)
        .all(|pair| matches!(pair, [lower, upper] if upper > lower))
}

/// Expands an encoded list back to individual sequences.
///
/// Upstream's `decodeSeqRanges`, with its entry limit preserved as [`SeqRangeError::TooMany`]: a
/// range is two numbers that can expand to billions, so the caller states the largest list it is
/// prepared to accept.
///
/// # Errors
///
/// Rejects a range whose end precedes its start, a run that would exceed `max_entries`, and an
/// encoding that is not strictly increasing once expanded.
pub fn decode_seq_ranges(
    encoded: &[EncodedSeq],
    max_entries: u64,
) -> Result<Vec<u64>, SeqRangeError> {
    let mut decoded: Vec<u64> = Vec::new();
    let mut had_range = false;
    for entry in encoded {
        match *entry {
            EncodedSeq::Single(value) => {
                if u64::try_from(decoded.len()).unwrap_or(u64::MAX) >= max_entries {
                    return Err(SeqRangeError::TooMany { max: max_entries });
                }
                decoded.push(value);
            }
            EncodedSeq::Range { start, end } => {
                if end < start {
                    return Err(SeqRangeError::Backwards { start, end });
                }
                let length = end - start + 1;
                let already = u64::try_from(decoded.len()).unwrap_or(u64::MAX);
                if length > max_entries.saturating_sub(already) {
                    return Err(SeqRangeError::TooMany { max: max_entries });
                }
                for value in start..=end {
                    decoded.push(value);
                }
                had_range = true;
            }
        }
    }
    if had_range && !is_strictly_increasing(&decoded) {
        return Err(SeqRangeError::NotIncreasing);
    }
    Ok(decoded)
}

/// Why an encoded sequence list could not be expanded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SeqRangeError {
    /// A range's end preceded its start.
    #[error("range end {end} precedes start {start}")]
    Backwards {
        /// The range start.
        start: u64,
        /// The range end.
        end: u64,
    },

    /// Expansion would produce more entries than the caller allows.
    #[error("decoded sequence list would exceed the {max}-entry limit")]
    TooMany {
        /// The caller's limit.
        max: u64,
    },

    /// The expanded list was not strictly increasing.
    #[error("decoded sequences must be strictly increasing")]
    NotIncreasing,
}
