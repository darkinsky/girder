//! The `sessionStats` projection unit: whole-log turn/step counts and wall times.
//!
//! Upstream path: `packages/session/session-stats/src` — 299 lines across four files. The whole
//! package is one capability with three layers, and only the middle one needed a runtime:
//!
//! | upstream file | kind | what it is |
//! |---|---|---|
//! | `types.ts` | pure declaration | `SessionStatsProjection` — the eight-field view, and the `sessionStats` projection-key augmentation |
//! | `projection.ts` | **pure fold over events** | the totals, the in-flight boundaries, and the two zod schemas |
//! | `index.ts` | service | a Cordis plugin: `name`, `inject: ['sessionProjections']`, and one `register` call |
//! | `client.ts` | re-export | `export type * from './types.ts'` |
//!
//! `index.ts` and `client.ts` are 40 lines between them and contain no logic: the plugin registers a
//! definition and the client namespace re-exports the same types. The capability lives entirely in
//! `projection.ts`, which is a fold from `(state, event)` to `state` — the exact shape
//! [`girder_core::Projection`] already models. So this module ports the fold, the two schemas, and
//! the registry adapter, and reproduces the plugin's two constants ([`NAME`], [`INJECT`]).
//!
//! # What this crate had, and why it was not this
//!
//! [`girder_core::SessionStats`] (`session_stats.rs`) counts turns, steps, tool calls, messages,
//! and tokens. It is not a partial version of this module: it answers a *different question* and
//! says so in its own header — every duration is deliberately absent because
//! [`girder_core::SessionEvent`] carries no timestamp. Reporting `llm_ms: 0` would tell a reader the
//! model was instant rather than that nothing measured it.
//!
//! [`girder_core::session::metrics`] renders those counts as Prometheus samples. That is the fleet
//! question ("what is the deployment doing"), not the conversation question.
//!
//! Upstream's eight fields split cleanly along that line:
//!
//! - **Two are counts** — `turns`, `steps`. `SessionStats` already answers both, with the same
//!   definitions: a turn counts only if it closed a step, and `step/end` is the counted step event
//!   rather than `assistant/message`.
//! - **Four are wall times** — `llmMs`, `toolMs`, `ttftMs`, `decodeMs`. Nothing in this crate
//!   computed any of them, because the log has no clock.
//! - **Two are decode accounting** — `ttftSteps`, `decodeTokens`. `SessionStats.output_tokens`
//!   totals every usage record; `decodeTokens` totals only the ones that closed a timed step, which
//!   is a strictly smaller set.
//!
//! So the missing capability is **the timed fold**: durations over a log that does not carry time.
//!
//! # How a fold with no clock in the log still measures time
//!
//! The time arrives from the caller, per event, exactly as
//! [`girder_core::session::query_plan`] already does it for event records: a `time_of`-shaped source
//! rather than a fabricated timestamp. Two sources exist here:
//!
//! - [`StatsInput`] for a caller folding a slice it already has in hand — tests, an importer, a
//!   replay that read timings from a sidecar. Every input carries `Option<i64>`, so a caller with
//!   no clock for one event says so instead of passing zero.
//! - [`TimeSource`] for the registry adapter ([`StatsProjection`]), which resolves `seq -> time`
//!   for each event the [`girder_core::ProjectionRegistry`] hands it.
//!
//! This is the one place the port deliberately differs from upstream, and the difference is
//! visible rather than silent: an unmeasurable boundary does **not** contribute a zero to a total.
//! It increments [`StatsState::unmeasured_durations`], which is state-only — the wire view keeps
//! upstream's exact eight fields — and answers "how many of these figures are known to be
//! incomplete". Without it, `llmMs: 0` would be indistinguishable from two things it is not: a
//! measured zero, and a fold that never had a clock.
//!
//! # Three more places the local event vocabulary forced a decision
//!
//! - **Pairing a message with its step.** Upstream matches `assistant/message` to its open step by
//!   `(turn, step)` read out of the event. [`girder_core::SessionEvent::AssistantMessage`] carries
//!   no turn and no step, so the fold pairs positionally: the message closes whatever step is
//!   open. That is the same rule in the only case upstream can produce (one message per step, the
//!   loop appends exactly one), and the defensive case — a duplicate message for a step that
//!   already closed — is ignored rather than double-counted, which is what upstream's explicit
//!   `openStep = null` accomplishes.
//! - **Pairing usage with its step.** Upstream reads `usage.outputTokens` off the same
//!   `assistant/message`. Here [`girder_core::SessionEvent::ModelUsage`] is a *separate* event that
//!   the loop appends immediately after the message, so the closed step leaves a
//!   [`DecodeAnchor`] behind and the usage event settles it. A usage record with no anchor (a
//!   re-reported settlement, or a log that begins mid-turn) accrues nothing.
//! - **Pairing a tool result with its call.** Upstream digs the id out of
//!   `message.source.callId` and then has to guard `Object.hasOwn` against prototype property names
//!   (`constructor`, `toString`) being read as inherited dispatch times.
//!   [`girder_core::SessionEvent::ToolResult`] carries the id directly and
//!   [`std::collections::BTreeMap`] has no prototype chain, so the hazard is absent by construction
//!   rather than by a check.
//!
//! # What is deliberately not here
//!
//! - **No Cordis plugin.** `index.ts` registers the definition on `ctx.sessionProjections` and does
//!   nothing else. [`StatsProjection`] is the registerable value and [`INJECT`] names the seam, but
//!   the registration itself belongs to whatever owns the registry — see the integration test.
//! - **No `client.ts` counterpart.** It is `export type * from './types.ts'`: a namespace
//!   projection with zero content. [`StatsView`] is the single definition both consumers read, and
//!   Rust needs no second path to it.
//! - **No zod.** The two schemas are `#[serde(deny_unknown_fields)]` derive plus the explicit
//!   non-negative check in [`StatsView::from_json`] and [`StatsState::from_json`], which is the
//!   same contract (`strict()`, `nonnegative()`, `int()`) with the type system carrying the
//!   integer part: `u64` cannot hold a fraction and `i64` cannot hold `1.5`.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use girder_core::llm::attempt::{StreamRecord, first_token_time};
use girder_core::{Projection, ProjectionSeq as Seq, SessionEvent, VersionedProjection};

/// The Cordis plugin name upstream registers under.
///
/// Carried because it is part of the plugin's identity on the wire and in diagnostics: two plugins
/// under one name cannot both be loaded, and a log that says `session-stats` should name the same
/// thing in both harnesses.
pub const NAME: &str = "session-stats";

/// The service the plugin cannot work without.
///
/// Upstream's `inject`: without a projection registry there is nowhere to register the definition,
/// so the plugin stays pending rather than loading and silently doing nothing.
pub const INJECT: [&str; 1] = ["sessionProjections"];

/// The projection key, which is also the field name in a session view.
pub const KEY: &str = "sessionStats";

/// The version of the persisted fold state.
///
/// Upstream's `stateVersion: 1`. A stored state folded under a different version is discarded and
/// refolded, which is the only safe response to a changed fold meaning.
pub const STATE_VERSION: u64 = 1;

/// The business facts of one open step: when it started, and when its first token arrived.
///
/// `first_token_ms` is `None` until an attempt reports one, and stays `None` for a step that never
/// streamed anything — which is why the view counts `ttft_steps` separately from `steps`: an
/// average first-token latency is only defined over the steps that had one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenStep {
    /// The turn this step belongs to.
    pub turn: u64,
    /// The step number within the turn.
    pub step: u64,
    /// When the step started, when the caller could date it.
    #[serde(default)]
    pub start_ms: Option<i64>,
    /// When the model's first output token arrived, once an attempt reported it.
    #[serde(default)]
    pub first_token_ms: Option<i64>,
}

/// The boundary a settled `assistant/message` leaves for its `model/usage` to fill in.
///
/// Upstream reads usage off the assembling message; this log records it as a following event. The
/// anchor is what lets the two meet without the fold having to look ahead, and it is cleared by the
/// first of `model/usage`, `step/end`, or `turn/end` so a re-reported settlement cannot settle
/// twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecodeAnchor {
    /// The step's first-token time.
    pub first_token_ms: i64,
    /// When that step's message assembled, when the caller could date it.
    #[serde(default)]
    pub message_ms: Option<i64>,
}

/// The fold state: the totals plus the in-flight boundaries they accrue from.
///
/// Plain JSON per the unit contract, because a resolved projection may be persisted — see
/// [`girder_core::VersionedProjection`]. The view is a strict subset of this state, which is why
/// [`StatsState::view`] exists rather than a second struct with the same eight fields and no
/// boundaries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsState {
    /// Distinct turns that closed at least one step.
    pub turns: u64,
    /// Closed steps, counting completed, failed, and cancelled steps alike.
    pub steps: u64,
    /// Summed model wall time over the steps that assembled a message, in milliseconds.
    pub llm_ms: i64,
    /// Summed tool wall time over matched `tool/call` → `tool/result` pairs, in milliseconds.
    pub tool_ms: i64,
    /// Summed first-token latency over [`Self::ttft_steps`], in milliseconds.
    pub ttft_ms: i64,
    /// Steps that carried a recorded first token.
    pub ttft_steps: u64,
    /// Summed decode wall time over the steps that also reported output tokens.
    pub decode_ms: i64,
    /// Summed provider output tokens over those same steps.
    pub decode_tokens: u64,
    /// The turn of the last counted `step/end`, or `None` before the first.
    ///
    /// One slot rather than a set, because turn numbers are assigned by the host and are monotonic
    /// per session: "is this the first closed step of a new turn" is a comparison against the
    /// highest turn seen, which is what keeps the state O(1) and persistable.
    pub last_turn: Option<u64>,
    /// The open step's boundary facts, or `None` outside a step or after its message assembled.
    pub open_step: Option<OpenStep>,
    /// Dispatch times of tool calls whose result has not landed, by call id.
    pub pending_calls: BTreeMap<String, Option<i64>>,
    /// The boundary a just-assembled message left for its usage record.
    #[serde(default)]
    pub decode_anchor: Option<DecodeAnchor>,
    /// How many timed boundaries the fold could not measure.
    ///
    /// State-only, never on the wire. Each step whose message assembled without a datable start or
    /// end, and each matched tool pair with an undatable side, adds one. It is the difference
    /// between "these figures are zero" and "these figures are incomplete", and a caller that
    /// renders a duration beside it is rendering something it can defend.
    #[serde(default)]
    pub unmeasured_durations: u64,
}

/// The wire view: upstream's eight fields and nothing else.
///
/// A separate type from [`StatsState`] rather than a projection of it, because the two have
/// different contracts: the view is what a client renders and is stable, the state carries in-flight
/// boundaries and may grow. The boundary fields would be a lie on the wire — a client has no use for
/// a dispatch time — and keeping them out is what makes the strict JSON check meaningful.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsView {
    /// Distinct turns that closed at least one step.
    pub turns: u64,
    /// Closed steps.
    pub steps: u64,
    /// Summed model wall time, milliseconds.
    pub llm_ms: i64,
    /// Summed tool wall time, milliseconds.
    pub tool_ms: i64,
    /// Summed first-token latency, milliseconds.
    pub ttft_ms: i64,
    /// Steps carrying a recorded first token.
    pub ttft_steps: u64,
    /// Summed decode wall time, milliseconds.
    pub decode_ms: i64,
    /// Summed provider output tokens over the decode-timed steps.
    pub decode_tokens: u64,
}

/// Why a stored or received value is not a session-stats state or view.
///
/// Every variant means "this value must not be trusted", never "retry with the same input": each one
/// is a shape the producer built and the consumer cannot interpret. The caller's response is to
/// refold from the log rather than to repair the value.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SchemaError {
    /// The value's shape does not match the schema.
    ///
    /// Carries the decoder's message rather than restating every way a shape can be wrong: the
    /// derive knows a missing key from an unknown one and a wrong type from a wrong length, and
    /// reproducing that list here would be a second, drifting definition.
    #[error("session stats shape: {0}")]
    Shape(String),

    /// A field that must be non-negative held a negative value.
    ///
    /// The one check the type system cannot make: `llmMs` is a duration and `-1` decodes into an
    /// `i64` perfectly well, so it has to be refused after decoding rather than before.
    #[error("session stats field `{field}` is negative ({value})")]
    Negative {
        /// The offending field.
        field: &'static str,
        /// The value it held.
        value: i64,
    },
}

/// One thing the fold reads: a durable event with its time, or a live attempt with its stream.
///
/// Two variants rather than one because the attempt is not a durable event in this crate: upstream
/// records `assistant/attempt` carrying the stream, and
/// [`girder_core::SessionEvent`] has no such variant. Making it a fold *input* instead of inventing
/// an event keeps the log vocabulary untouched while still being able to pin a first-token time
/// before the message lands.
#[derive(Clone, Copy, Debug)]
pub enum StatsInput<'a> {
    /// A durable event, with the time the caller could establish for it and the attempt stream that
    /// produced it when the caller holds one.
    Event {
        /// The event.
        event: &'a SessionEvent,
        /// When it happened, in milliseconds, when the caller can date it.
        at_ms: Option<i64>,
        /// The stream the model produced for this step, when the caller holds one.
        ///
        /// Only read for a message: it is the fallback source of the first-token time when no
        /// attempt reported one, which is upstream's `open.firstTokenTime ?? firstToken(event.stream)`.
        stream: Option<&'a [StreamRecord]>,
    },

    /// A live attempt frame: the step it belongs to and the stream it is producing.
    ///
    /// Its time is deliberately absent. Upstream's `assistant/attempt` case reads only the stream —
    /// the step's start is the boundary, not the attempt — so carrying a time here would invite a
    /// caller to think one was used.
    Attempt {
        /// The turn the step belongs to.
        turn: u64,
        /// The step number.
        step: u64,
        /// The stream so far.
        stream: &'a [StreamRecord],
    },
}

impl StatsState {
    /// The state of an empty log.
    ///
    /// Every field zero and every boundary absent, which is what
    /// [`girder_core::Projection::init`] has to answer: an empty log has a state, and two callers
    /// guessing at it would guess differently.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The wire view: exactly the eight fields upstream publishes.
    ///
    /// The boundary fields are dropped rather than serialised and ignored, so a client cannot come
    /// to depend on internal state that has no contract.
    #[must_use]
    pub fn view(&self) -> StatsView {
        StatsView {
            turns: self.turns,
            steps: self.steps,
            llm_ms: self.llm_ms,
            tool_ms: self.tool_ms,
            ttft_ms: self.ttft_ms,
            ttft_steps: self.ttft_steps,
            decode_ms: self.decode_ms,
            decode_tokens: self.decode_tokens,
        }
    }

    /// Whether any duration in this state is known to be incomplete.
    ///
    /// The question a renderer asks before showing a per-step average: an average over a state with
    /// unmeasured steps is an average over the steps that happened to be datable.
    #[must_use]
    pub fn has_unmeasured_durations(&self) -> bool {
        self.unmeasured_durations > 0
    }

    /// Decodes a state, rejecting a negative duration.
    ///
    /// # Errors
    ///
    /// [`SchemaError::Shape`] for a value the schema does not describe, [`SchemaError::Negative`]
    /// for a negative duration.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, SchemaError> {
        let state: Self = serde_json::from_value(value.clone())
            .map_err(|error| SchemaError::Shape(error.to_string()))?;
        state.validate()?;
        Ok(state)
    }

    /// Encodes the state, for a persisted checkpoint.
    ///
    /// # Errors
    ///
    /// [`SchemaError::Shape`] when the value cannot be represented, which is unreachable for this
    /// struct — a map of strings to numbers and options. The result is reported rather than
    /// unwrapped because "unreachable" is a claim the encoder, not this function, is making.
    pub fn to_json(&self) -> Result<serde_json::Value, SchemaError> {
        serde_json::to_value(self).map_err(|error| SchemaError::Shape(error.to_string()))
    }

    /// Refuses a negative duration anywhere in the state.
    fn validate(&self) -> Result<(), SchemaError> {
        for (field, value) in [
            ("llmMs", self.llm_ms),
            ("toolMs", self.tool_ms),
            ("ttftMs", self.ttft_ms),
            ("decodeMs", self.decode_ms),
        ] {
            if value < 0 {
                return Err(SchemaError::Negative { field, value });
            }
        }
        if let Some(step) = &self.open_step {
            for (field, value) in [
                ("openStep.startMs", step.start_ms),
                ("openStep.firstTokenMs", step.first_token_ms),
            ] {
                if let Some(value) = value
                    && value < 0
                {
                    return Err(SchemaError::Negative { field, value });
                }
            }
        }
        if let Some(anchor) = &self.decode_anchor
            && anchor.first_token_ms < 0
        {
            return Err(SchemaError::Negative {
                field: "decodeAnchor.firstTokenMs",
                value: anchor.first_token_ms,
            });
        }
        Ok(())
    }
}

impl StatsView {
    /// Decodes a view, rejecting a negative duration and any field the schema does not name.
    ///
    /// This is upstream's `sessionStatsSchema` — `strict()`, `nonnegative()`, `int()` — with the
    /// integer part carried by `u64` and the strictness by `deny_unknown_fields`.
    ///
    /// # Errors
    ///
    /// [`SchemaError::Shape`] for an unknown key, a missing key, or a wrong type;
    /// [`SchemaError::Negative`] for a negative duration.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, SchemaError> {
        let view: Self = serde_json::from_value(value.clone())
            .map_err(|error| SchemaError::Shape(error.to_string()))?;
        for (field, amount) in [
            ("llmMs", view.llm_ms),
            ("toolMs", view.tool_ms),
            ("ttftMs", view.ttft_ms),
            ("decodeMs", view.decode_ms),
        ] {
            if amount < 0 {
                return Err(SchemaError::Negative {
                    field,
                    value: amount,
                });
            }
        }
        Ok(view)
    }

    /// Encodes the view as the object a client receives.
    ///
    /// # Errors
    ///
    /// As [`StatsState::to_json`].
    ///
    /// Takes `self` by value: the view is eight integers and `Copy`, so a caller that reads the
    /// encoded form has nothing left to use afterwards, and passing a reference would let a borrow
    /// outlive the encoding for no benefit.
    pub fn to_json(self) -> Result<serde_json::Value, SchemaError> {
        serde_json::to_value(self).map_err(|error| SchemaError::Shape(error.to_string()))
    }
}

/// Folds one input into a state.
///
/// The unit of the whole module, and a pure function: no clock, no I/O, no lookups. Everything about
/// *when* things happened is in `input`, which is what makes a replay and a live fold agree.
///
/// An input the fold does not care about returns an equal state rather than the same reference. That
/// is the one place this port is weaker than upstream, whose `Object.is` gate lets the change feed
/// skip an unchanged fold; here the registry compares sequence numbers, so an equal state costs an
/// allocation and nothing else.
#[must_use]
pub fn apply(state: &StatsState, input: &StatsInput<'_>) -> StatsState {
    let mut next = state.clone();
    match input {
        StatsInput::Event {
            event,
            at_ms,
            stream,
        } => apply_event(&mut next, event, *at_ms, *stream),
        StatsInput::Attempt { turn, step, stream } => {
            apply_attempt(&mut next, *turn, *step, stream);
        }
    }
    next
}

/// Folds a slice of inputs from the empty state.
#[must_use]
pub fn fold(inputs: &[StatsInput<'_>]) -> StatsState {
    inputs
        .iter()
        .fold(StatsState::new(), |state, input| apply(&state, input))
}

/// Why one durable event does not move the fold.
fn apply_event(
    state: &mut StatsState,
    event: &SessionEvent,
    at_ms: Option<i64>,
    stream: Option<&[StreamRecord]>,
) {
    match event {
        SessionEvent::StepStart { turn, step } => {
            state.open_step = Some(OpenStep {
                turn: *turn,
                step: *step,
                start_ms: at_ms,
                first_token_ms: None,
            });
        }
        SessionEvent::AssistantMessage { .. } => close_message(state, at_ms, stream),
        SessionEvent::ModelUsage { output_tokens, .. } => accrue_decode(state, *output_tokens),
        SessionEvent::ToolCall { id, .. } => {
            state.pending_calls.insert(id.as_str().to_owned(), at_ms);
        }
        SessionEvent::ToolResult { id, .. } => close_tool(state, id.as_str(), at_ms),
        SessionEvent::StepEnd { turn, .. } => close_step(state, *turn),
        SessionEvent::TurnEnd { .. } => {
            // A call whose result never landed belongs to a cancelled or failed turn. Results always
            // land within their turn, so the leftovers are dropped rather than kept in persisted
            // state forever.
            state.pending_calls.clear();
            state.decode_anchor = None;
        }
        _ => {}
    }
}

/// Pins the open step's first token from a live attempt.
///
/// Only the *first* attempt that reports one counts. A retry inside the same step produces a second
/// stream whose first token is later than the step's real latency boundary, so taking the later one
/// would report a retry's delay as the model's.
fn apply_attempt(state: &mut StatsState, turn: u64, step: u64, stream: &[StreamRecord]) {
    let Some(open) = state.open_step.as_mut() else {
        return;
    };
    if open.turn != turn || open.step != step || open.first_token_ms.is_some() {
        return;
    }
    if let Some(first) = first_token_time(stream) {
        open.first_token_ms = Some(first);
    }
}

/// Closes the open step at an assembled message: model time, first token, and the anchor for usage.
///
/// The boundary is taken rather than read, so a duplicate message for a step that already closed
/// finds no open step and accrues nothing. That is upstream's "one assembled message per step"
/// guarantee, and it is what makes an over-eager retry harmless.
fn close_message(state: &mut StatsState, at_ms: Option<i64>, stream: Option<&[StreamRecord]>) {
    let Some(open) = state.open_step.take() else {
        return;
    };

    // `Math.max(0, …)` upstream: a message cannot precede its step, but a clock that jumped can make
    // it look as though it did, and a negative duration would corrupt every total it entered.
    match (open.start_ms, at_ms) {
        (Some(start), Some(now)) => state.llm_ms += (now - start).max(0),
        _ => state.unmeasured_durations += 1,
    }

    let first = open
        .first_token_ms
        .or_else(|| stream.and_then(first_token_time));
    let Some(first) = first else {
        return;
    };
    state.ttft_steps += 1;
    if let Some(start) = open.start_ms {
        state.ttft_ms += (first - start).max(0);
    }
    state.decode_anchor = Some(DecodeAnchor {
        first_token_ms: first,
        message_ms: at_ms,
    });
}

/// Settles the anchor a closed message left, when a usage record follows it.
///
/// Decode time and decode tokens are accrued **together**, as upstream does: a token count with no
/// measurable decode window would be a figure whose partner is zero, and a caller dividing one by
/// the other would be dividing by a number nothing measured.
fn accrue_decode(state: &mut StatsState, output_tokens: u64) {
    let Some(anchor) = state.decode_anchor.take() else {
        // A re-reported settlement, or a log that starts mid-turn. Counted as unmeasurable so the
        // tokens are not silently dropped from the story the state tells.
        state.unmeasured_durations += 1;
        return;
    };
    let Some(message_ms) = anchor.message_ms else {
        state.unmeasured_durations += 1;
        return;
    };
    state.decode_ms += (message_ms - anchor.first_token_ms).max(0);
    state.decode_tokens = state.decode_tokens.saturating_add(output_tokens);
}

/// Matches a tool result against its dispatch, by call id.
///
/// Matched on the removal rather than checked after it: a result for a call this log never recorded
/// is not a tool pair, and counting its wall time would make the total describe work the log does
/// not contain. The map lookup is also why upstream's prototype-hazard guard has no analogue here —
/// a `BTreeMap` key named `constructor` is just a key.
fn close_tool(state: &mut StatsState, call_id: &str, at_ms: Option<i64>) {
    let Some(dispatched) = state.pending_calls.remove(call_id) else {
        return;
    };
    match (dispatched, at_ms) {
        (Some(start), Some(now)) => state.tool_ms += (now - start).max(0),
        _ => state.unmeasured_durations += 1,
    }
}

/// Counts a closed step, and the turn it was the first closed step of.
///
/// `step/end` is the authority rather than `assistant/message`: the loop appends exactly one per
/// entered step in a `finally`, so completed, failed, cancelled, and max-tokens steps all land one.
/// Counting messages instead would overcount usage-host steps and undercount cancelled ones.
fn close_step(state: &mut StatsState, turn: u64) {
    if state.last_turn != Some(turn) {
        state.turns += 1;
        state.last_turn = Some(turn);
    }
    state.steps += 1;
    // A cancelled step assembles no message; its partial stream time stays uncounted in every time
    // figure, which is why clearing here does not accrue anything.
    state.open_step = None;
    state.decode_anchor = None;
}

/// Where the fold's times come from, for the registry adapter.
///
/// A trait rather than a closure because two things are needed per event and one of them is
/// optional: a stream is only interesting for the message that assembled it. Implementors are
/// cheap and immutable; the adapter holds one `Arc` and never asks it anything else.
pub trait TimeSource: Send + Sync {
    /// When the event at `seq` happened, in milliseconds, when the source can date it.
    ///
    /// `None` is a real answer — "this log carries no clock" — and never a stand-in for zero.
    fn at_ms(&self, seq: Seq) -> Option<i64>;

    /// The model stream the event at `seq` produced, when the source kept one.
    ///
    /// `None` for every seq by default: a source that dates events without keeping their streams is
    /// the common case, and the fold then falls back to the timeline it can build from times alone.
    fn stream_at(&self, _seq: Seq) -> Option<Vec<StreamRecord>> {
        None
    }
}

/// A source that cannot date anything.
///
/// The default a caller with a log but no clock should pass: counts and turn/step structure fold
/// correctly, every duration stays zero, and [`StatsState::unmeasured_durations`] says so. Handing
/// out fabricated times instead would produce the same zero totals while claiming they were
/// measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct Untimed;

impl TimeSource for Untimed {
    fn at_ms(&self, _seq: Seq) -> Option<i64> {
        None
    }
}

/// A source over timings and streams recorded out of band.
///
/// The shape a sidecar index, an importer, or a test uses: sequence number in, timing (and
/// optionally the step's stream) out. Keys are 1-based, matching [`girder_core::ProjectionSeq`].
#[derive(Clone, Debug, Default)]
pub struct Recorded {
    times: BTreeMap<Seq, i64>,
    streams: BTreeMap<Seq, Vec<StreamRecord>>,
}

impl Recorded {
    /// An empty recording.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the time of the event at `seq`.
    #[must_use]
    pub fn with_time(mut self, seq: Seq, at_ms: i64) -> Self {
        self.times.insert(seq, at_ms);
        self
    }

    /// Records the stream produced by the event at `seq`.
    #[must_use]
    pub fn with_stream(mut self, seq: Seq, stream: Vec<StreamRecord>) -> Self {
        self.streams.insert(seq, stream);
        self
    }
}

impl TimeSource for Recorded {
    fn at_ms(&self, seq: Seq) -> Option<i64> {
        self.times.get(&seq).copied()
    }

    fn stream_at(&self, seq: Seq) -> Option<Vec<StreamRecord>> {
        self.streams.get(&seq).cloned()
    }
}

/// The registerable `sessionStats` unit.
///
/// Upstream's `sessionStatsProjectionDefinition` as a value that
/// [`girder_core::ProjectionRegistry::register`] accepts. The fold is the same one [`apply`]
/// performs; the adapter's only job is to turn a `seq` into a time, which is the one thing upstream
/// got for free from `event.time`.
pub struct StatsProjection {
    source: Arc<dyn TimeSource>,
}

impl StatsProjection {
    /// A unit folded over `source`.
    #[must_use]
    pub fn new(source: Arc<dyn TimeSource>) -> Self {
        Self { source }
    }

    /// A unit over a log that carries no clock.
    ///
    /// Counts and structure are exact; every duration is zero and
    /// [`StatsState::unmeasured_durations`] counts the boundaries that could not be dated.
    #[must_use]
    pub fn untimed() -> Self {
        Self::new(Arc::new(Untimed))
    }
}

impl std::fmt::Debug for StatsProjection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StatsProjection")
            .field("key", &KEY)
            .field("state_version", &STATE_VERSION)
            .finish_non_exhaustive()
    }
}

impl Projection for StatsProjection {
    fn key(&self) -> &'static str {
        KEY
    }

    fn init(&self) -> serde_json::Value {
        StatsState::new()
            .to_json()
            .unwrap_or(serde_json::Value::Null)
    }

    fn apply(&self, state: serde_json::Value, event: &SessionEvent, seq: Seq) -> serde_json::Value {
        // A state that does not parse is left untouched rather than replaced by `init`: rewriting it
        // would silently discard a fold that was almost readable, and the registry's cursor has
        // already moved past the event that would have rebuilt it.
        let Ok(current) = StatsState::from_json(&state) else {
            return state;
        };
        let stream = self.source.stream_at(seq);
        let input = StatsInput::Event {
            event,
            at_ms: self.source.at_ms(seq),
            stream: stream.as_deref(),
        };
        apply(&current, &input).to_json().unwrap_or(state)
    }

    fn in_summary(&self) -> bool {
        // Upstream's `summary: false`: no list row renders these figures, so a summary payload that
        // carried them would pay for them on every listing and use none.
        false
    }
}

impl VersionedProjection for StatsProjection {
    fn state_version(&self) -> u64 {
        STATE_VERSION
    }
}
