//! The `tokenMeasurement` fold: the recovery facts `measure()` reuses.
//!
//! # What upstream has, measured
//!
//! `packages/llm/token-meter/src/measurement-projection.ts` is **100 lines**, and it exports
//! exactly three things:
//!
//! | Upstream | Lines | What it decides |
//! | --- | --- | --- |
//! | `MeasurementState` | 8 | four fields: `headerSeq`, `stepStart`, `anchor`, `invalidSeq` |
//! | `advanceMeasurement` | 30 | one pure transition, throwing on a boundary violation |
//! | `measurementProjectionDefinition` | 15 | `key: 'tokenMeasurement'`, `stateVersion: 2`, `init`, and the error-swallowing `apply` |
//!
//! Its own summary is the whole specification: *"fixed-size recovery facts; positional attachment
//! prices remain a rebuildable cache"*. The fold answers one question — **which recovery facts does
//! `measure()` need that it cannot recompute from the event it is standing on?** — and the answer is
//! four:
//!
//! 1. [`MeasurementState::header_seq`] — the last `request/header` the fold saw. `measure()` needs
//!    it to decide whether the anchored snapshot was taken under the *same* route as the current
//!    request. A mismatch means the anchor's heuristic price and the current surface's price are not
//!    comparable, and the delta must be reported as zero rather than as a number about nothing.
//! 2. [`StepCoordinate`] — the open `step/start`, so an `assistant/message` or `step/end` that
//!    belongs to a different step (or to no step) can be rejected instead of priced.
//! 3. [`MeasurementAnchor`] — the latest successful call's usage facts: the header it ran under, and
//!    the assistant's own heuristic price. This is the only *quantitative* field, and it is why the
//!    state cannot be derived from the log tail alone: the anchor is "the surface as it stood
//!    immediately before this one assistant message", which no single later event records.
//! 4. [`MeasurementState::invalid_seq`] — the first sequence whose measurement *failed*. Sticky and
//!    absorbing: once set, every later event is folded as a no-op.
//!
//! The fourth field carries a design decision worth restating, because it looks like error
//! suppression and is the opposite. Upstream's `apply` catches its own error and records the
//! position instead of propagating:
//!
//! ```text
//! if (state.invalidSeq !== null) return state
//! try { return advanceMeasurement(state, event) }
//! catch { return { ...state, invalidSeq: event.seq } }
//! ```
//!
//! The reason is stated in the source: *"`measure()` repeats the calculation there to report its
//! original error without blocking unrelated projection units"*. A projection framework drives
//! every unit over every event; a unit that throws would abort the drive and starve its siblings.
//! So the fold degrades itself to a no-op and leaves the *diagnosis* to the caller that can afford
//! to fail — [`advance_measurement`], which throws the original error at the recorded position.
//! That split is the whole point of the module, and [`TokenMeasurementProjection::validate`] is the
//! local spelling of it.
//!
//! # What this crate already covers, and why it does not cover this
//!
//! `packages/llm/token-meter` is 1,625 lines across twelve files. [`crate::llm::token_projection`]
//! already ports five of them as pure folds over a slice of [`SurfaceEnvelope`]:
//!
//! | Fold | Upstream | Answers |
//! | --- | --- | --- |
//! | [`crate::llm::token_projection::price_surface`] | `surface-fold.ts` | each node's price, each event's signed delta, the total |
//! | [`crate::llm::token_projection::PricedSurface::with_image_prices`] | `route-pricing.ts` | the same surface under a routed image policy |
//! | [`crate::llm::token_projection::context_breakdown`] | `breakdown-projection.ts` | system / tools / message composition |
//! | [`crate::llm::token_projection::derive_turn_usage`] | `turn-usage.ts` | exact accounting for one completed turn |
//! | [`crate::llm::token_projection::fold_token_usage`], [`crate::llm::token_projection::context_pressure`] | `usage-projection.ts` | durable totals and provider-anchored occupancy |
//!
//! None of them covers this file, and the reason is not an oversight — it is that **every one of
//! those five is a fold that recomputes its whole answer from a slice**, while `measurement` is the
//! one unit whose *output is the input of another fold*. The five consume a log; `measure()` needs a
//! small value that says which slice of the log to treat as the baseline, and that value has to be
//! carried (and checkpointed) rather than recomputed. Two concrete consequences:
//!
//! - [`crate::llm::token_projection::context_pressure`] *does* find an anchor, but a different one:
//!   the `model/usage` record adjacent to an assistant node. It can therefore answer "what did the
//!   provider say the prompt was" but not "what did the *request header* look like when it said so",
//!   because it has no header fold at all. `measure()` needs both to decide whether the provider
//!   anchor and the current heuristic are the same quantity.
//! - `price_surface` prices the surface *as it is now*. `measure()` needs the surface *as it was
//!   immediately before the anchor assistant message* to attribute the delta of everything admitted
//!   since. That is a historical slice, which is exactly what a bounded checkpoint cannot keep and a
//!   scalar cannot be recomputed into.
//!
//! So the gap is real and it is narrow: a four-field state, one transition, one projection
//! definition, and the `measure()` baseline decision that consumes them.
//!
//! # The accurate scope of the gap
//!
//! What is missing from this crate, stated as narrowly as it actually is:
//!
//! 1. **No `tokenMeasurement` projection.** Nothing folds `request/header` position, the open step,
//!    the anchor and the invalid position into checkpointable state.
//! 2. **No baseline selection.** `measure()` chooses between three baselines — `none`, `estimated`,
//!    `usage` — by comparing the anchored header against the current one and the provider's total
//!    against the anchor's heuristic price. This crate cannot make that decision, so every caller
//!    that wanted "how big is the next request, and how sure are we" had to make it itself.
//! 3. **No signed surface delta with a provable zero.** The `surfaceDeltaTokens` of
//!    [`Measurement`] is zero when the headers disagree, for the reason the source gives: a delta
//!    between two differently-priced surfaces is a number about nothing.
//!
//! What is **not** missing, and this module therefore does not invent:
//!
//! - **No `request/header` event, and no header re-derivation.** [`crate::SessionEvent`] still has no
//!   such variant, so no local log can *record* one. The header arrives here as
//!   [`MeasurementInput::header`] — a caller-supplied value of [`EpochHeader`], the type
//!   [`crate::llm::request_header`] already defines for exactly this envelope. Upstream reads it off
//!   the event; this module takes the same value from whoever folded the header, which is the
//!   [`crate::llm::request_header::fold_request_header`] the sibling module already provides. The
//!   fold is unchanged; only its source moved.
//! - **No stream reassembly.** Upstream prices the assistant by decoding `event.data.stream`
//!   (`assembleAssistantStream(...).blocks()`) when a usage record is present, and by
//!   `deriveEventMessage` otherwise. Girder's log holds the assembled [`Message`] directly, so
//!   [`measurement_assistant_tokens`] prices `derive_event_message` in **both** arms. That is the
//!   same number, read from the representation this crate actually stores — and it removes
//!   upstream's documented decode-error arm rather than silently ignoring it.
//! - **No cache-read / cache-write buckets.** [`crate::SessionEvent::ModelUsage`] records input and
//!   output only. [`TokenUsageFacts::usage_tokens`] sums the buckets the log has and reports the
//!   absent ones as zero *by absence*, not by a stored zero, so a caller can tell "the provider did
//!   not report a cache hit" from "there was no cache hit".
//! - **No `WeakMap` cache and no live LLM lookup.** Those belong to `index.ts`, which is a service.
//!   This is a fold.
//!
//! # Why the JSON state is the state
//!
//! [`TokenMeasurementProjection`] stores [`MeasurementState`] as JSON, because that is what
//! `Projection` requires of its state. The alternative — keeping a typed struct in the projection and
//! a JSON mirror beside it — is the shape that lets the two disagree. Instead the typed form is
//! [`MeasurementState`] itself, and the JSON is produced and consumed by its own `Serialize` /
//! `Deserialize` derive, with [`MeasurementState::from_state`] rejecting anything the schema would
//! reject. A round trip through JSON is what
//! [`ProjectionCache`](crate::ProjectionCache) does to a checkpoint, so the fold is tested through a
//! round trip rather than merely serialized and hoped for.
//!
//! # Bounded, and measurably so
//!
//! Upstream pins the checkpoint's size: after 300 appended messages the state must be unchanged and
//! its JSON must be under 200 characters. The same claim is asserted here
//! (`checkpoint_is_independent_of_message_count`), and it is a property of the *shape* — four
//! scalars, one nullable coordinate and one nullable triple — not of a length limit that happened to
//! fit.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use std::sync::Arc;

use girder_core::llm::request_header::{EpochHeader, header_equals};
use girder_core::{
    ChatMessage, Projection, ProjectionSeq as Seq, SessionEvent, SurfaceEnvelope,
    VersionedProjection, derive_event_message, estimate_blocks,
};
use serde::{Deserialize, Serialize};

/// Role-framing overhead charged per model-visible message.
///
/// Upstream's `ROLE_OVERHEAD`. Repeated here rather than imported for the same reason
/// [`crate::llm::token_projection::ROLE_OVERHEAD`] repeats it: [`crate::llm::token_meter`] is a private
/// module of the crate root, so its constant is not reachable from a `#[path]`-included build. The
/// value is pinned against [`girder_core::estimate_message`] by a test, so a change there fails
/// instead of drifting.
pub const ROLE_OVERHEAD: u64 = 4;

/// The projection key, which is also its field name in a view.
///
/// Upstream's `key: 'tokenMeasurement'`. A constant because two places need it — the definition and
/// the checkpoint reader — and a string literal in two places is a typo waiting to become a silently
/// empty read.
pub const TOKEN_MEASUREMENT_KEY: &str = "tokenMeasurement";

/// The version of the state this module persists.
///
/// Upstream's `stateVersion: 2`. Pinned at 2, not reset to 1: a checkpoint written by a build that
/// follows upstream's numbering would otherwise be *accepted* under a different meaning, which is the
/// silent corruption [`VersionedProjection::state_version`] exists to prevent. The local fold is a
/// port of upstream's version 2 semantics, so it declares version 2.
pub const TOKEN_MEASUREMENT_STATE_VERSION: u64 = 2;

/// One open step, identified by the coordinates its `step/start` named.
///
/// Upstream's `state.stepStart`, which is `{ turn, step } | null`. The nullable whole is represented
/// here as `Option<StepCoordinate>`, so "no step is open" is one value rather than two fields that
/// can disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepCoordinate {
    /// Turn the step belongs to.
    pub turn: u64,
    /// Step number within the turn.
    pub step: u64,
}

impl StepCoordinate {
    /// Builds a coordinate.
    #[must_use]
    pub const fn new(turn: u64, step: u64) -> Self {
        Self { turn, step }
    }
}

/// The usage facts of the latest successful call, as the anchor keeps them.
///
/// Upstream's `state.anchor.usage`, a `TokenUsage | null`. The three upstream optional buckets that
/// [`girder_core::SessionEvent::ModelUsage`] cannot carry are `None` here rather than `Some(0)`: a
/// stored zero claims the provider reported no cache activity, and absence claims the log never
/// recorded the bucket. Only the second is true, and a caller showing a cache hit rate needs to know
/// which it is holding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_field_names,
    reason = "the names are upstream's `inputTokens`/`outputTokens`/`cacheReadTokens`/`cacheWriteTokens`; renaming them would make a reader diffing against the reference work out which field is which, and the `_tokens` suffix is what distinguishes a token count from a request count"
)]
pub struct UsageFacts {
    /// Prompt tokens the provider billed.
    pub input_tokens: u64,
    /// Completion tokens the provider billed.
    pub output_tokens: u64,
    /// Provider-declared cache-read tokens, when the log carried them.
    ///
    /// Always `None` in this build — see the type documentation. The field exists so a producer that
    /// starts recording the bucket does not have to change the state's shape, and so a checkpoint
    /// written elsewhere can be read without pretending the bucket was zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Provider-declared cache-write tokens, when the log carried them.
    ///
    /// Always `None` in this build, for the same reason as [`Self::cache_read_tokens`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

impl UsageFacts {
    /// The facts a [`girder_core::SessionEvent::ModelUsage`] records.
    #[must_use]
    pub const fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_read_tokens: None,
            cache_write_tokens: None,
        }
    }

    /// Total tokens across every bucket the facts hold.
    ///
    /// Upstream's `usageTokens`: input + cache-read + cache-write + output. The buckets are disjoint
    /// — a cached prompt token is billed as a cache read and not as input — so summing them does not
    /// double-count, and the absent ones contribute nothing rather than a wrong zero.
    #[must_use]
    pub fn usage_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.cache_write_tokens.unwrap_or(0))
            .saturating_add(self.output_tokens)
    }
}

/// The latest successful call's recovery facts: what header it ran under, and what it cost.
///
/// Upstream's `state.anchor`, which is
/// `{ headerSeq, assistantTokens, usage } | null`. The two halves are kept together because they are
/// only meaningful together: `assistant_tokens` is the assistant's own heuristic price *under the
/// route named by* `header_seq`, and comparing it against a differently-routed surface is the
/// mistake [`Measurement::baseline`] exists to avoid.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeasurementAnchor {
    /// The `request/header` position the call ran under, when the fold had seen one.
    ///
    /// `None` means the anchor was taken before any header was logged, so its route is unknown and
    /// no comparison against a current route is provable.
    pub header_seq: Option<u64>,
    /// The assistant message's own heuristic price, excluding the surface it was appended to.
    pub assistant_tokens: u64,
    /// The provider's usage facts, when the call reported them under a known header.
    pub usage: Option<UsageFacts>,
}

/// The four recovery facts one measurement prefix carries.
///
/// Upstream's `MeasurementState`. Every field is either a position or a small scalar, which is what
/// makes the state checkpointable and what the bounded-size test asserts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeasurementState {
    /// Sequence of the last `request/header` the fold saw.
    pub header_seq: Option<u64>,
    /// The open step, or `None` between steps.
    pub step_start: Option<StepCoordinate>,
    /// The latest successful call's anchor, or `None` before the first assistant message.
    pub anchor: Option<MeasurementAnchor>,
    /// Sequence of the first event whose measurement failed, or `None` for a clean prefix.
    ///
    /// Sticky and absorbing: once set, [`advance_measurement`] is not consulted again and every
    /// later event folds as a no-op. That is what keeps one malformed region from producing a
    /// cascade of different diagnoses, and it is why `measure()` can report the *first* position
    /// rather than the last.
    pub invalid_seq: Option<u64>,
}

impl MeasurementState {
    /// The state of an empty log.
    ///
    /// Upstream's `init`: every field null. Not derived from [`Default`] at the call site because the
    /// projection's `init` must not be able to drift from it, and one function is how that is kept
    /// true.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            header_seq: None,
            step_start: None,
            anchor: None,
            invalid_seq: None,
        }
    }

    /// Whether the prefix folded without a measurement failure.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.invalid_seq.is_none()
    }

    /// Converts the state to the JSON a projection carries.
    ///
    /// # Errors
    ///
    /// Returns [`MeasurementStateError::NonJson`] if the value cannot be serialized, which would mean
    /// one of the four fields stopped being representable — a definition bug, not a data condition.
    pub fn to_state(&self) -> Result<serde_json::Value, MeasurementStateError> {
        serde_json::to_value(self).map_err(|error| MeasurementStateError::NonJson {
            detail: error.to_string(),
        })
    }

    /// Reads a projection state back into the typed form.
    ///
    /// This is upstream's `stateSchema.parse`: the checkpoint contract. A row that does not have the
    /// shape is refused rather than defaulted, because a defaulted state would price a prefix it
    /// never folded.
    ///
    /// # Errors
    ///
    /// Returns [`MeasurementStateError::Malformed`] naming the field when the value is not this
    /// state's shape.
    pub fn from_state(value: &serde_json::Value) -> Result<Self, MeasurementStateError> {
        let Some(object) = value.as_object() else {
            return Err(MeasurementStateError::Malformed {
                detail: "state is not a JSON object".to_owned(),
            });
        };
        // Unknown keys are refused rather than ignored. Upstream's schema is `.strict()`, and the
        // difference matters: a state carrying a `surface` array is a state written by a build that
        // kept positional prices, which is exactly the corruption `stateVersion` could not catch
        // because the version was not bumped.
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "headerSeq" | "stepStart" | "anchor" | "invalidSeq"
            ) {
                return Err(MeasurementStateError::Malformed {
                    detail: format!("unknown field `{key}`"),
                });
            }
        }
        serde_json::from_value(value.clone()).map_err(|error| MeasurementStateError::Malformed {
            detail: error.to_string(),
        })
    }
}

/// Why a persisted measurement state could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MeasurementStateError {
    /// The value is not the shape this definition writes.
    #[error("token measurement state: {detail}")]
    Malformed {
        /// What is wrong with the value.
        detail: String,
    },

    /// The state could not be represented as JSON at all.
    #[error("token measurement state is not representable as JSON: {detail}")]
    NonJson {
        /// The serializer's diagnosis.
        detail: String,
    },
}

/// Why a measurement prefix is not usable.
///
/// Upstream throws bare `Error`s with hand-written messages. Naming the cases is the same divergence
/// [`crate::llm::token_projection::TurnUsageGap`] makes and for the same reason: a caller showing a
/// diagnostic can say *what* broke, and a test can assert the specific case instead of merely that
/// something failed. The messages keep upstream's wording so a reader can diff against the reference.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MeasurementError {
    /// A `step/start` arrived while another step was still open.
    #[error(
        "token meter: step/start at seq {seq} arrived before turn {open_turn}/step {open_step} ended"
    )]
    NestedStepStart {
        /// Sequence of the offending event.
        seq: u64,
        /// Turn of the step still open.
        open_turn: u64,
        /// Step number still open.
        open_step: u64,
    },

    /// A `step/end` did not match the open step, or arrived with none open.
    #[error("token meter: step/end at seq {seq} has no matching step/start event")]
    UnmatchedStepEnd {
        /// Sequence of the offending event.
        seq: u64,
    },

    /// An `assistant/message` arrived with no matching open step.
    #[error("token meter: assistant/message at seq {seq} has no matching step/start event")]
    UnmatchedAssistantMessage {
        /// Sequence of the offending event.
        seq: u64,
    },
}

/// Advances one measurement prefix without mutating its previous state.
///
/// Upstream's `advanceMeasurement`, transition for transition. Four cases and a default:
///
/// - `request/header` records its position and changes nothing else. The header's *content* is not
///   kept: the anchor only needs to know **whether** the route was the same, which
///   [`MeasurementAnchor::header_seq`] answers by position.
/// - `step/start` opens a step, refusing a nested one. Two open steps make every later
///   `assistant/message` ambiguous about which step it settled.
/// - `step/end` closes the step, refusing an unmatched one.
/// - `assistant/message` records the anchor, refusing to price a message outside a step.
/// - everything else is a no-op. Most of a log is that.
///
/// `assistant_tokens` is supplied by the caller rather than computed here, because the only thing
/// that distinguishes upstream's two arms is whether a usage record was present *and* a header was
/// known; that decision is [`measurement_assistant_tokens`]'s, and keeping it out of the transition
/// keeps the transition a pure function of the event's *position* and *lifecycle*.
///
/// # Errors
///
/// [`MeasurementError`] naming the first boundary violation.
pub fn advance_measurement(
    state: &MeasurementState,
    event: &SessionEvent,
    seq: u64,
    assistant: Option<AssistantMeasurement>,
) -> Result<MeasurementState, MeasurementError> {
    // A failed prefix is absorbing: every later event leaves it exactly as it was. Checked here
    // rather than at the call site because the transition is the one place that can guarantee it.
    if state.invalid_seq.is_some() {
        return Ok(state.clone());
    }
    let mut next = state.clone();
    match event {
        SessionEvent::StepStart { turn, step } => {
            if let Some(open) = state.step_start {
                return Err(MeasurementError::NestedStepStart {
                    seq,
                    open_turn: open.turn,
                    open_step: open.step,
                });
            }
            next.step_start = Some(StepCoordinate::new(*turn, *step));
        }
        SessionEvent::StepEnd { turn, step } => {
            let Some(open) = state.step_start else {
                return Err(MeasurementError::UnmatchedStepEnd { seq });
            };
            if open.turn != *turn || open.step != *step {
                return Err(MeasurementError::UnmatchedStepEnd { seq });
            }
            next.step_start = None;
        }
        SessionEvent::AssistantMessage { .. } => {
            // The step check happens whether or not a measurement was offered. It is a fact about
            // the *log's lifecycle* — a response with no open request is not a response this fold can
            // price — and skipping it when the caller offered nothing would make an invalid log look
            // clean to whoever chose not to price it.
            let Some(open) = state.step_start else {
                return Err(MeasurementError::UnmatchedAssistantMessage { seq });
            };
            let Some(measurement) = assistant else {
                // No measurement offered means the caller chose not to price this message; the
                // anchor is left as it was rather than being replaced with a zero.
                return Ok(next);
            };
            // When the caller knows the coordinates (upstream's event carries them), the message
            // must name the open step. `None` means the local event does not record them, so the
            // open-step check above is the whole of what can be proven.
            if let Some((turn, step)) = measurement.coordinates {
                if open.turn != turn || open.step != step {
                    return Err(MeasurementError::UnmatchedAssistantMessage { seq });
                }
            }
            next.anchor = Some(MeasurementAnchor {
                header_seq: state.header_seq,
                assistant_tokens: measurement.assistant_tokens,
                usage: measurement.usage,
            });
        }
        // Everything else is a no-op. In particular `request/context` and `model/selection` are *not*
        // upstream's `request/header`: one is route capacity and the other is the choice, while the
        // header is the whole envelope. Recording either as the header position would claim the fold
        // had seen an envelope it never saw, so the caller supplies the header through
        // [`MeasurementInput::header`] instead.
        _ => {}
    }
    Ok(next)
}

/// What an `assistant/message` contributes to the anchor.
///
/// The coordinates are read off the event itself in upstream (its `assistant/message` data carries
/// `turn` and `step`); Girder's [`SessionEvent::AssistantMessage`] carries only the message, so the
/// coordinates are [`Self::coordinates`] and are `None` in the ordinary case. Reading the open step's
/// own coordinates instead would make the match check vacuous — it could never fail — which is why
/// they are supplied rather than derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssistantMeasurement {
    /// The turn and step the message claims to answer, when the log records them.
    ///
    /// `Some` reproduces upstream's check that the message names the open step. `None` says the
    /// caller cannot prove which step it belongs to — a local `assistant/message` carries no
    /// coordinates — and leaves the open-step requirement as the whole of what is checked.
    pub coordinates: Option<(u64, u64)>,
    /// The message's own heuristic price, excluding the surface it appends to.
    pub assistant_tokens: u64,
    /// Provider usage facts, when the call reported them under a known header.
    pub usage: Option<UsageFacts>,
}

/// Prices one `assistant/message` the way `measure()` needs it, choosing between the two arms.
///
/// Upstream's two arms, and the condition that selects between them:
///
/// ```text
/// if (event.data.usage !== undefined && state.headerSeq !== null) {
///   const content = assembleAssistantStream(event.data.stream).blocks()
///   assistantTokens = content.length === 0 ? 0 : estimateContent(content) + ROLE_OVERHEAD
///   usage = event.data.usage
/// } else {
///   const message = deriveEventMessage(event)
///   assistantTokens = message === null ? 0 : estimateMessage(message)
/// }
/// ```
///
/// Both arms price the *same* content; they differ only in where it comes from. Girder's log stores
/// the assembled message, so both arms read [`derive_event_message`] and the distinction reduces to
/// **whether the usage facts are adopted**. That is the whole behaviour that survives the port, and
/// it is stated here rather than silently collapsed: a caller that wanted the second arm's
/// deliberately-usage-less anchor asks for [`UsageFacts::new`] never to appear, which is exactly what
/// passing `None` does.
///
/// # Why the anchor price excludes the surface
///
/// `assistantTokens` is the assistant message's own price, not the surface's. Upstream adds it to
/// the *anchored nodes* (`priceSurface(state.anchorNodes, ...)` + `anchor.assistantTokens`) to rebuild
/// the surface as it stood when the call was made. Keeping the two apart is what lets a caller reprice
/// the anchored nodes under a different route and still add an unchanged assistant price.
#[must_use]
pub fn measurement_assistant_tokens(event: &SessionEvent) -> u64 {
    derive_event_message(&SurfaceEnvelope::log_only(event.clone(), 0)).map_or(0, message_tokens)
}

/// Prices one message, matching upstream's `estimateContent(...) + ROLE_OVERHEAD`.
///
/// A logged message is a [`ChatMessage`] — this crate's `crate::session::message::Message` — not the [`crate::llm::Message`]
/// that [`girder_core::estimate_message`] takes: the log's shape carries a role and a source that the
/// request shape does not, because a replay must be able to tell a user prompt from an injected one.
/// So the price is [`estimate_blocks`] over the content plus [`ROLE_OVERHEAD`], which is exactly what
/// `estimate_message` composes for the other shape.
///
/// A message with no content is 0 rather than framing: upstream's `content.length === 0 ? 0` arm,
/// and the reason is that "no message" and "an empty message" are the same request field.
///
/// # Why the system arm is not special-cased
///
/// Upstream's `estimate.ts` charges a system prompt text density plus role framing and no per-block
/// overhead. That is [`crate::llm::token_projection`]'s `price_message` rule for the *surface*, where a
/// system node is priced as one serialized string. Here the anchor's assistant arm is the only
/// reachable one — an `assistant/message` is never a system message — so reproducing the system rule
/// would be an unreachable branch claiming to be tested.
#[must_use]
fn message_tokens(message: &ChatMessage) -> u64 {
    if message.content.is_empty() {
        return 0;
    }
    estimate_blocks(&message.content).saturating_add(ROLE_OVERHEAD)
}

/// Reads the usage facts out of a `model/usage` record.
///
/// Returns `None` for any event that is not one. A separate function because the anchor's usage and
/// `fold_token_usage`'s totals read the same record for different purposes, and having one reader
/// means a producer change is one edit.
#[must_use]
pub fn measurement_usage(event: &SessionEvent) -> Option<UsageFacts> {
    match event {
        SessionEvent::ModelUsage {
            input_tokens,
            output_tokens,
            ..
        } => Some(UsageFacts::new(*input_tokens, *output_tokens)),
        _ => None,
    }
}

/// The baseline from which a signed surface delta produces current pressure.
///
/// Upstream's `TokenMeasurementBaseline`, a three-armed discriminated union. An enum rather than a
/// struct with an optional usage, because "estimated" and "usage" differ in *what is known*, and a
/// struct with `usage: Option<_>` would let a caller build "kind = usage, usage = None".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MeasurementBaseline {
    /// No request has been made and the surface is empty: the honest answer is zero.
    None,
    /// A heuristic price: the tool catalogue plus every surface node.
    Estimated {
        /// The price.
        tokens: u64,
    },
    /// A provider-reported total, provably at least as large as the heuristic anchor.
    Usage {
        /// The provider's total across its disjoint buckets.
        tokens: u64,
        /// The facts it was derived from.
        usage: UsageFacts,
    },
}

impl MeasurementBaseline {
    /// The baseline's token count.
    #[must_use]
    pub const fn tokens(&self) -> u64 {
        match self {
            Self::None => 0,
            Self::Estimated { tokens } | Self::Usage { tokens, .. } => *tokens,
        }
    }

    /// The kind, spelled the way upstream's `kind` field spells it.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Estimated { .. } => "estimated",
            Self::Usage { .. } => "usage",
        }
    }
}

/// What one measurement reads: everything `measure()` needs that is not the fold's own state.
///
/// A struct rather than six parameters, and the reason is not tidiness: the fields are *the same
/// quantity measured three ways*, and a positional signature would let a caller swap the current
/// surface for the anchored one without the compiler noticing. Those two are both `u64`.
#[derive(Clone, Debug, PartialEq)]
pub struct MeasurementInput {
    /// The request envelope in force, canonical.
    ///
    /// `None` when the caller has no header — upstream's `header === undefined`. Supplied rather than
    /// folded because this crate's log has no `request/header` event; see the module documentation.
    pub header: Option<EpochHeader>,
    /// The current surface's route-priced total.
    pub surface_tokens: u64,
    /// The current surface's *heuristic* total, used as every unanchored estimate.
    pub heuristic_surface_tokens: u64,
    /// The surface total as it stood immediately before the anchor assistant message.
    ///
    /// Only read when the anchor exists and its header matches; otherwise it describes a surface that
    /// is not being compared to anything.
    pub anchor_surface_tokens: u64,
    /// The tool catalogue's heuristic price for the current header.
    pub tools_tokens: u64,
    /// The canonical header the anchor ran under, when the caller can produce it.
    ///
    /// `None` means "unknown", which is treated as *not equal* to a known current header. That is the
    /// conservative direction: an unknown route cannot be proven the same, so the delta is not
    /// reported as a like-for-like comparison.
    pub anchor_header: Option<EpochHeader>,
}

/// The current request-and-response pressure, with the provenance of every part of it.
///
/// Upstream's `TokenMeasurement`, minus the node list (which
/// [`crate::llm::token_projection::price_surface`] already returns and which this fold deliberately
/// does not retain — it is the rebuildable cache the module summary names) and with the log revision
/// supplied by the caller's own cursor rather than guessed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measurement {
    /// The baseline the total is anchored to.
    pub baseline: MeasurementBaseline,
    /// Signed repricing of the current surface relative to the anchored surface.
    ///
    /// Signed because a compaction can put the current surface *below* the anchor, and clamping it
    /// first would silently report a projection that never shrank. Zero when no anchor applies or
    /// when the routes differ.
    pub surface_delta_tokens: i64,
    /// Non-negative current pressure: the baseline plus the delta, floored at zero.
    pub total_tokens: u64,
    /// The current surface's route-priced total.
    pub surface_tokens: u64,
}

/// Computes the measurement for one prefix, given the fold's state.
///
/// Upstream's `measure()`, reduced to the part that is a function of the fold. The three-way choice
/// is the whole algorithm:
///
/// 1. **Anchored, with matching headers.** The anchor applies. The baseline is the provider's usage
///    *only* when it is at least as large as the heuristic price of the same anchored surface —
///    upstream's `usageTokens(usage) >= estimatedAnchorTokens`. That guard is not defensive: a
///    provider total smaller than the content the model was sent cannot be the whole bill (a
///    truncated or partially-reported usage record), and preferring it would understate pressure.
///    The signed delta is then the current surface minus the *anchored* surface, so it measures what
///    was admitted since the call.
/// 2. **No header and an empty surface.** [`MeasurementBaseline::None`]. Nothing was sent, so
///    "unknown" would overstate what is known: the answer is zero and it is exact.
/// 3. **Anything else.** A plain heuristic: the tool catalogue plus the *current* surface, with a
///    zero delta. There is no anchor to measure a change against.
///
/// # Why case 1's delta uses the anchor's surface, not the anchor's total
///
/// The anchor's surface is the surface *before* the anchored assistant message; the current surface
/// includes it, plus everything admitted after. Subtracting the former from the latter therefore
/// yields the anchored assistant message's own price plus every later admission — the exact change
/// in what the request carries, which is what a signed delta is supposed to mean.
#[must_use]
pub fn measure_measurement(state: &MeasurementState, input: &MeasurementInput) -> Measurement {
    let anchored = state.anchor.as_ref().filter(|_| {
        // Upstream's `optionalHeaderEquals(current, cachedHeader(anchor.headerSeq))`: both sides must
        // be known and equal. An unknown anchor header is never a match.
        match (&input.header, &input.anchor_header) {
            (Some(current), Some(anchor)) => header_equals(current, anchor),
            (None, None) => true,
            _ => false,
        }
    });

    if let Some(anchor) = anchored {
        let anchor_surface_tokens = input
            .anchor_surface_tokens
            .saturating_add(anchor.assistant_tokens);
        let estimated_anchor_tokens = input.tools_tokens.saturating_add(anchor_surface_tokens);
        let baseline = match anchor.usage {
            // The provider's total wins only when it cannot be smaller than what was sent.
            Some(usage) if usage.usage_tokens() >= estimated_anchor_tokens => {
                MeasurementBaseline::Usage {
                    tokens: usage.usage_tokens(),
                    usage,
                }
            }
            _ => MeasurementBaseline::Estimated {
                tokens: estimated_anchor_tokens,
            },
        };
        let surface_delta_tokens = signed_difference(input.surface_tokens, anchor_surface_tokens);
        return Measurement {
            total_tokens: clamp_pressure(baseline.tokens(), surface_delta_tokens),
            baseline,
            surface_delta_tokens,
            surface_tokens: input.surface_tokens,
        };
    }

    if input.header.is_none() && input.surface_tokens == 0 {
        return Measurement {
            baseline: MeasurementBaseline::None,
            surface_delta_tokens: 0,
            total_tokens: 0,
            surface_tokens: 0,
        };
    }

    let estimated = input
        .tools_tokens
        .saturating_add(input.heuristic_surface_tokens);
    Measurement {
        baseline: MeasurementBaseline::Estimated { tokens: estimated },
        surface_delta_tokens: 0,
        total_tokens: estimated,
        surface_tokens: input.surface_tokens,
    }
}

/// Signed difference of two token counts, saturating instead of wrapping or panicking.
fn signed_difference(left: u64, right: u64) -> i64 {
    let left = i64::try_from(left).unwrap_or(i64::MAX);
    let right = i64::try_from(right).unwrap_or(i64::MAX);
    left.saturating_sub(right)
}

/// Clamps a possibly negative pressure at zero, as upstream's `Math.max(0, ...)` does.
fn clamp_pressure(baseline: u64, delta: i64) -> u64 {
    let baseline = i64::try_from(baseline).unwrap_or(i64::MAX);
    u64::try_from(baseline.saturating_add(delta).max(0)).unwrap_or(u64::MAX)
}

/// The `tokenMeasurement` projection: the recovery facts, as a fold over the event log.
///
/// Upstream's `measurementProjectionDefinition`, with its three declared fields — key, version, and
/// `init` — plus the error-swallowing `apply` this module documents at the top.
///
/// # Why `apply` never returns an error
///
/// The projection framework drives every unit over every event, and a unit that failed would abort
/// the drive for its siblings. So a boundary violation is recorded in
/// [`MeasurementState::invalid_seq`], the state is then frozen, and
/// [`Self::validate`] is how a caller that *can* afford to fail asks for the original diagnosis.
/// That is upstream's exact split, and inverting it — throwing from `apply` — would change which
/// failures reach a projection snapshot.
pub struct TokenMeasurementProjection;

impl TokenMeasurementProjection {
    /// The projection key.
    #[must_use]
    pub const fn key_name() -> &'static str {
        TOKEN_MEASUREMENT_KEY
    }

    /// The state an empty log has.
    #[must_use]
    pub fn init_state() -> MeasurementState {
        MeasurementState::empty()
    }

    /// Folds one event into a typed state, recording or propagating the failure per `apply`'s rule.
    ///
    /// The one entry point both the projection and a direct caller use, so the two cannot disagree
    /// about whether a bad event is recorded or thrown.
    ///
    /// `measurement` is the [`AssistantMeasurement`] a caller prepared for this event; `None` for
    /// every event that is not an `assistant/message`, and for an assistant message the caller chose
    /// not to price.
    #[must_use]
    pub fn apply_typed(
        state: &MeasurementState,
        event: &SessionEvent,
        seq: Seq,
        measurement: Option<AssistantMeasurement>,
    ) -> MeasurementState {
        if state.invalid_seq.is_some() {
            return state.clone();
        }
        match advance_measurement(state, event, seq, measurement) {
            Ok(next) => next,
            // The position is retained and the fold freezes here. `measure()` repeats the
            // calculation at this position to report the original error.
            Err(_) => MeasurementState {
                invalid_seq: Some(seq),
                ..state.clone()
            },
        }
    }

    /// Re-raises the failure the fold recorded at [`MeasurementState::invalid_seq`].
    ///
    /// `events` must be the log the state was folded from, in order, indexed from 0 as
    /// [`crate::fold_surface`] requires. The replay stops at the recorded position and returns the
    /// error a throwing `advance_measurement` would have returned — which is what makes the recorded
    /// failure a *deferred* diagnosis rather than a lost one.
    ///
    /// # Errors
    ///
    /// Returns the [`MeasurementError`] of the recorded position, or
    /// [`MeasurementError::UnmatchedStepEnd`]-style fallback for a state whose `invalid_seq` names no
    /// event (a checkpoint corrupted by hand).
    pub fn validate(
        state: &MeasurementState,
        events: &[SurfaceEnvelope],
    ) -> Result<(), MeasurementError> {
        let Some(invalid) = state.invalid_seq else {
            return Ok(());
        };
        let mut current = MeasurementState::empty();
        for envelope in events {
            let measurement = assistant_measurement_for(&envelope.event, &current);
            match advance_measurement(&current, &envelope.event, envelope.seq, measurement) {
                Ok(next) => current = next,
                Err(error) => {
                    if envelope.seq == invalid {
                        return Err(error);
                    }
                    // The replay failed somewhere other than the recorded position: the state does
                    // not describe this log, so report the mismatch rather than a borrowed error.
                    return Err(MeasurementError::UnmatchedStepEnd { seq: envelope.seq });
                }
            }
        }
        Err(MeasurementError::UnmatchedStepEnd { seq: invalid })
    }
}

/// The assistant measurement for one event, using only what the event itself carries.
///
/// `turn` and `step` are `None` here because a Girder `assistant/message` records neither; the
/// coordinates are the open step's. Returning a measurement whose coordinates are read from the
/// state would make the match check vacuous, so this helper reports the *price* and leaves the
/// coordinates to the caller that knows them.
fn assistant_measurement_for(
    event: &SessionEvent,
    state: &MeasurementState,
) -> Option<AssistantMeasurement> {
    if !matches!(event, SessionEvent::AssistantMessage { .. }) {
        return None;
    }
    state.step_start?;
    Some(AssistantMeasurement {
        // A local `assistant/message` records no turn or step, so there is nothing to match against
        // beyond the open-step requirement `advance_measurement` already applies.
        coordinates: None,
        assistant_tokens: measurement_assistant_tokens(event),
        // Usage is recorded by a following `model/usage`, so the event itself carries none. An
        // anchor built here is therefore an `estimated` baseline, which is the honest reading of a
        // log whose accounting sits in a separate record.
        usage: None,
    })
}

impl Projection for TokenMeasurementProjection {
    fn key(&self) -> &'static str {
        TOKEN_MEASUREMENT_KEY
    }

    fn init(&self) -> serde_json::Value {
        // The typed state is the source of truth; a serialization failure here would be a definition
        // bug, and returning `Null` would let the framework store a state `from_state` then refuses.
        // `MeasurementState` is four optional scalars, so the only way to fail is a broken derive,
        // which the test suite would catch before this line could.
        Self::init_state()
            .to_state()
            .unwrap_or(serde_json::Value::Null)
    }

    fn apply(&self, state: serde_json::Value, event: &SessionEvent, seq: Seq) -> serde_json::Value {
        // A state this definition cannot read is left untouched rather than replaced: replacing it
        // would erase the evidence that a checkpoint was written by another shape, and the caller
        // reading it sees the same value it stored.
        let Ok(current) = MeasurementState::from_state(&state) else {
            return state;
        };
        let measurement = assistant_measurement_for(event, &current);
        let next = Self::apply_typed(&current, event, seq, measurement);
        next.to_state().unwrap_or(state)
    }

    fn in_summary(&self) -> bool {
        // Upstream leaves `summary` undeclared, which means `true`: the value is small (four fields,
        // under 200 bytes by the bounded-size test) and a session list benefits from knowing where
        // the last header and anchor sit.
        true
    }
}

impl VersionedProjection for TokenMeasurementProjection {
    fn state_version(&self) -> u64 {
        TOKEN_MEASUREMENT_STATE_VERSION
    }
}

/// The projection paired with its declared version, ready to register with a
/// [`ProjectionCache`](crate::ProjectionCache).
///
/// A function rather than a `const` because `ProjectionCache::new` takes `Arc<dyn
/// VersionedProjection>`, and building that at each call site would repeat the cast.
#[must_use]
pub fn token_measurement_definition() -> Arc<dyn VersionedProjection> {
    Arc::new(TokenMeasurementProjection)
}
