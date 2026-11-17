//! Token accounting folds: priced surfaces, per-turn usage, and context composition.
//!
//! # What upstream has
//!
//! `packages/llm/token-meter` is 1,625 lines across twelve files, and they are three different
//! kinds of thing:
//!
//! | upstream file | kind | what it answers |
//! |---|---|---|
//! | `estimate.ts` | pure arithmetic | heuristic price of a block, a message, a tool catalogue |
//! | `surface-fold.ts` | pure fold over events | per-node price of the retained surface, plus each event's signed delta |
//! | `surface-projection.ts` | pure O(1) fold | the surface *total* only, riding shadow-price claims |
//! | `route-pricing.ts` | pure fold over nodes | the same surface priced for a routed model's image policy |
//! | `usage-projection.ts` | pure fold over events | provider usage totalled, and context pressure |
//! | `breakdown-projection.ts` | pure fold over events | system / tools / message composition |
//! | `turn-usage.ts` | pure fold over one turn | exact provider accounting for one completed turn |
//! | `measurement-projection.ts` | pure fold over events | the recovery facts `measure()` reuses |
//! | `types.ts`, `projection.ts` | vocabulary | the shapes above |
//! | `index.ts` | **service** | a Cordis `Service` with a `WeakMap` cache, projection handles, and a live LLM lookup |
//! | `client.ts` | re-export | — |
//!
//! Only the last two rows need a runtime. Everything else is a fold from a slice of events to a
//! value, which is exactly the shape this crate can hold without a bus, a clock, or I/O.
//!
//! # What this crate had
//!
//! [`crate::token_meter`] ports `estimate.ts` (the fixed-density heuristic) and stops there, on
//! purpose: its documentation explains that it prices nothing and that `price_for` answers `None`
//! for every model. [`crate::cost_estimate`] then builds a *monetary* table on top of injected
//! rates, and [`crate::usage_quota`] builds a *windowed ceiling* over recorded counts. Those two
//! are not upstream's missing half; they answer questions upstream does not model at all. The
//! missing half is the accounting: nothing in this crate prices a retained surface, attributes
//! usage to a turn, or says what the next request's context is made of.
//!
//! # The gap this module closes
//!
//! Five folds, each a pure function from a slice of [`SurfaceEnvelope`] to a value:
//!
//! 1. [`price_surface`] — every retained node's heuristic price, each event's signed delta, and
//!    the total (`surface-fold.ts`).
//! 2. [`PricedSurface::with_image_prices`] — the same surface re-priced under a routed model's
//!    per-image policy, with the arity check upstream throws on (`route-pricing.ts`).
//! 3. [`context_breakdown`] — system / tools / message composition of the next request
//!    (`breakdown-projection.ts`).
//! 4. [`derive_turn_usage`] — exact provider accounting for one completed turn
//!    (`turn-usage.ts`), plus [`derive_turn_usages`] over a whole log.
//! 5. [`fold_token_usage`] and [`context_pressure`] — durable usage totals and the
//!    provider-anchored occupancy figure (`usage-projection.ts`).
//!
//! # Why keeping the nodes beats upstream's bounded state
//!
//! Upstream's *projections* keep scalar totals plus at most one shadow-price claim, because a
//! projection checkpoint must be O(1) and is persisted per event. That forces two documented
//! compromises: a replacement with no adjacent claim folds at zero delta, and the breakdown keeps
//! a `systems` list so it can subtract a claim's system prompts. Both exist to bound state, not
//! because they are right.
//!
//! [`crate::SurfaceFold`] already keeps the node list, so the folds here do too, and the
//! compromises disappear: a replacement subtracts the exact prices of the nodes it shadows, and
//! the breakdown subtracts the exact system nodes that were shadowed. The price is O(surface)
//! instead of O(1), which is the right trade for a caller that reads once per event and already
//! holds the surface — and it is what upstream's own `surface-fold.ts` does for measurement.
//!
//! # What is deliberately not here
//!
//! - **No service, no cache, no `WeakMap`.** `index.ts` is a Cordis service that resolves the live
//!   LLM route for image pricing and a file-projection callback. Both are lookups; a fold cannot
//!   make them, so the route prices arrive as a parameter here.
//! - **No file projection re-pricing.** Upstream's `route-pricing.ts` also replaces a file's
//!   structural price with the text the mounted LLM service projects it to. This crate has no
//!   `fileRequestText` and [`crate::ContentBlock::File`] carries only an opaque reference, so a
//!   file keeps its structural price. Images are re-priced because the provider price is derivable
//!   ([`crate::price_request_image`]) once the caller supplies the geometry.
//! - **No timers.** Nothing here reads a clock; `usage_quota` takes `now_ms` for the same reason.
//! - **No cache-write accounting.** [`crate::SessionEvent::ModelUsage`] records input and output
//!   only, so the cache-read bucket `estimate.ts` reports never reaches the log. Per-turn and
//!   session totals therefore cannot report it; that is a producer change, not a fold change, and
//!   [`TurnTokenUsage`] says so rather than reporting a zero that would look measured.
//! - **No tools from the log.** Upstream reads the tool catalogue out of the newest
//!   `request/header` event. This crate's log has no such event, so [`context_breakdown`] takes the
//!   catalogue as a parameter — the same figure, supplied by whoever assembled the request.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use girder_core::{
    ChatMessage, ChatRole, ContentBlock, SessionEvent, SurfaceEnvelope, SurfaceError, SurfaceOp,
    ToolSchema, derive_event_message, estimate_blocks, estimate_text, fold_surface,
    tool_result_content,
};

/// Structural JSON overhead charged per content block.
///
/// Upstream's `BLOCK_OVERHEAD`. Repeated here rather than imported because
/// [`crate::token_meter`] is a private module of the crate root and its constant is therefore not
/// reachable from a `#[path]`-included build. The value is pinned against the public estimator by
/// `tests/token_projection_gap.rs`, so a change there fails a test instead of drifting.
pub const BLOCK_OVERHEAD: u64 = 4;

/// Role-framing overhead charged per model-visible message.
///
/// Upstream's `ROLE_OVERHEAD`, with the same reason for being repeated. Only the tool-catalogue
/// price below and the per-message price use it, and the test pins it the same way.
pub const ROLE_OVERHEAD: u64 = 4;

/// Why a token fold could not produce an answer.
///
/// Two different failures share this type because a caller reacts to both the same way: the fold
/// refused, and the number it would have produced is not trustworthy. The surface variants carry
/// [`SurfaceError`] rather than restating it — surface placement is one contract, owned by
/// [`girder_core::fold_surface`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProjectionError {
    /// The log is not a valid surface, so no price can be attached to it.
    #[error("token surface: {0}")]
    Surface(#[from] SurfaceError),

    /// A routed price vector did not cover the surface's image occurrences one for one.
    ///
    /// Upstream throws here for the same reason: a misaligned vector would silently price node *n*
    /// with occurrence *n-1*'s image, which is a wrong number rather than a missing one.
    #[error(
        "token meter: route image pricing answered {given} prices for {needed} image occurrences"
    )]
    ImagePriceArity {
        /// Prices the caller supplied.
        given: usize,
        /// Occurrences the surface holds.
        needed: usize,
    },
}

/// One image occurrence retained on the surface, with the structural price a route replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageOccurrence {
    /// The opaque attachment reference, in message order.
    pub reference: String,
    /// What the fixed heuristic charged for the block's JSON structure.
    ///
    /// A route that can see the image subtracts this and adds its own visual price: the reference
    /// never reaches the model, so charging both would count the same block twice.
    pub structural_tokens: u64,
}

/// One priced node in the ordered surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PricedNode {
    /// Durable log position of the event that placed this node.
    pub seq: u64,
    /// Fixed-heuristic price of the node's message, independent of any route.
    pub heuristic_tokens: u64,
    /// Price under the route in force; equal to [`Self::heuristic_tokens`] until re-priced.
    pub tokens: u64,
    /// Whether this node came from a `system/message` event.
    ///
    /// The breakdown needs it to decide which price is the system prompt, and the log is the only
    /// place that fact lives.
    pub is_system: bool,
    /// Image occurrences in message order; empty for image-free nodes.
    pub images: Vec<ImageOccurrence>,
}

/// Where a surface event's node went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceTarget {
    /// The node joined the tail.
    Append,
    /// The node stands where the inclusive index range it shadowed stood.
    Replace {
        /// First shadowed node, as an index into the surface *before* this event.
        start_idx: usize,
        /// Last shadowed node, inclusive.
        end_idx: usize,
    },
}

/// One validated surface transition, as a fold reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceTransition {
    /// Sequence of the event that caused the transition.
    pub seq: u64,
    /// Heuristic price of the event's own message; 0 when it derives none.
    ///
    /// Heuristic even on a re-priced surface: a transition describes what *happened* to the surface,
    /// and the shadow-price protocol that reasons about replacements is defined on the fixed
    /// estimator.
    pub tokens: u64,
    /// Signed change the transition made to the heuristic surface total.
    ///
    /// Signed because a replacement can shrink the surface: a compaction that summarizes forty
    /// messages into one lowers the total, and a `u64` could not say so.
    pub delta_tokens: i64,
    /// What the transition did with the node.
    pub target: SurfaceTarget,
}

/// A priced surface: its nodes, its transitions, and its total.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PricedSurface {
    nodes: Vec<PricedNode>,
    transitions: Vec<SurfaceTransition>,
    surface_tokens: u64,
}

impl PricedSurface {
    /// The retained nodes, in model-visible order.
    #[must_use]
    pub fn nodes(&self) -> &[PricedNode] {
        &self.nodes
    }

    /// Every transition the fold applied, in log order.
    ///
    /// Retained because a delta is only meaningful next to the event that caused it: a reader
    /// asking "why did the surface shrink" gets the answer from the replacement's own row.
    #[must_use]
    pub fn transitions(&self) -> &[SurfaceTransition] {
        &self.transitions
    }

    /// The ambient total: the sum of [`PricedNode::tokens`] over [`Self::nodes`].
    #[must_use]
    pub const fn surface_tokens(&self) -> u64 {
        self.surface_tokens
    }

    /// The total under the fixed heuristic, ignoring any route pricing.
    ///
    /// Separate from [`Self::surface_tokens`] so a caller comparing a route against the heuristic
    /// does not have to re-sum the nodes.
    #[must_use]
    pub fn heuristic_surface_tokens(&self) -> u64 {
        self.nodes.iter().fold(0u64, |total, node| {
            total.saturating_add(node.heuristic_tokens)
        })
    }

    /// Re-prices every image occurrence under a routed model's policy.
    ///
    /// `prices` holds one visual-token price per image occurrence, in surface order — the shape
    /// upstream's `LlmImageRequestPricing.priceImages` answers in. `None` keeps the fixed
    /// heuristic, which is what a text-only route wants: it sees the reference as text, so the
    /// structural price is the right one.
    ///
    /// [`crate::price_request_image`] is how a caller turns a source geometry into one of these
    /// prices. It must do that for *every* retained image and in surface order; this function
    /// checks the count and refuses a mismatch rather than guessing which occurrence drifted.
    ///
    /// # Errors
    ///
    /// [`ProjectionError::ImagePriceArity`] when `prices` is present and its length differs from
    /// the number of image occurrences on the surface.
    pub fn with_image_prices(&self, prices: Option<&[u64]>) -> Result<Self, ProjectionError> {
        let needed = self.nodes.iter().fold(0usize, |count, node| {
            count.saturating_add(node.images.len())
        });
        let Some(prices) = prices else {
            return Ok(self.clone());
        };
        if prices.len() != needed {
            return Err(ProjectionError::ImagePriceArity {
                given: prices.len(),
                needed,
            });
        }
        let mut cursor = 0usize;
        let mut nodes = Vec::with_capacity(self.nodes.len());
        let mut surface_tokens = 0u64;
        for node in &self.nodes {
            let mut tokens = node.heuristic_tokens;
            for image in &node.images {
                let price = prices[cursor];
                cursor = cursor.saturating_add(1);
                tokens = tokens
                    .saturating_sub(image.structural_tokens)
                    .saturating_add(price);
            }
            surface_tokens = surface_tokens.saturating_add(tokens);
            nodes.push(PricedNode {
                tokens,
                ..node.clone()
            });
        }
        Ok(Self {
            nodes,
            transitions: self.transitions.clone(),
            surface_tokens,
        })
    }
}

/// Prices every retained surface node and records each event's signed delta.
///
/// The fold is the measurement path of `surface-fold.ts`: nodes keep their identity and their
/// heuristic price, the total is the sum of those prices, and a replacement subtracts the exact
/// price of the range it shadows rather than trusting a claim. Nothing is mutated and no state is
/// carried between calls, so a replay is reproducible by construction.
///
/// The log is validated by [`girder_core::fold_surface`] first. That is not defensive
/// duplication: placement is one contract, and a price attached to a log that is not a surface
/// would be a number about nothing.
///
/// # Errors
///
/// [`ProjectionError::Surface`] for any log [`girder_core::fold_surface`] refuses — non-contiguous
/// sequences, a placement on a non-surface event, a replacement range that is not on the surface,
/// provenance that omits a shadowed node, or an event that needs a message interpreter this crate
/// does not have.
pub fn price_surface(log: &[SurfaceEnvelope]) -> Result<PricedSurface, ProjectionError> {
    fold_surface(log)?;
    let mut builder = SurfaceBuilder::default();
    for envelope in log {
        builder.apply(envelope)?;
    }
    Ok(PricedSurface {
        nodes: builder.nodes,
        transitions: builder.transitions,
        surface_tokens: builder.surface_tokens,
    })
}

/// What the next request's context is made of, in heuristic tokens.
///
/// Upstream's `ContextBreakdownProjection`. The three figures are *composition*, not cost: each
/// uses the fixed density heuristic, so they do not add up to a provider-anchored
/// [`ContextPressure::projected_tokens`], and presenting either as the other is the mistake the
/// upstream README warns about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_field_names,
    reason = "the names are upstream's `systemTokens`/`toolsTokens`/`messageTokens`; renaming them would make a reader diffing against the reference work out which field is which"
)]
pub struct ContextBreakdown {
    /// Heuristic price of the last nonempty surviving system prompt, in surface order; 0 when the
    /// surface holds none.
    ///
    /// "Surviving" matters: a compaction that shadows the prompt removes it from this figure, and
    /// the next assembled prompt replaces it.
    pub system_tokens: u64,
    /// Heuristic price of the newest request envelope's tool schemas; 0 before any request.
    pub tools_tokens: u64,
    /// Heuristic price of every other visible node, including superseded system prompts.
    pub message_tokens: u64,
}

/// Composes the next request's context out of the retained surface and one tool catalogue.
///
/// `tools` is the catalogue the newest request carried — this crate's log does not record one, so
/// the caller that assembled the request supplies it. An empty slice means "no tools", which
/// prices at zero rather than at [`BLOCK_OVERHEAD`].
///
/// Upstream applies no route pricing to this figure, and neither does this: composition is a
/// property of the prompt, and an image's visual price is a property of the route.
///
/// # Errors
///
/// [`ProjectionError::Surface`] when the log is not a valid surface.
pub fn context_breakdown(
    log: &[SurfaceEnvelope],
    tools: &[ToolSchema],
) -> Result<ContextBreakdown, ProjectionError> {
    let surface = price_surface(log)?;
    let system_tokens = surface
        .nodes()
        .iter()
        .rev()
        .find(|node| node.is_system && node.heuristic_tokens > 0)
        .map_or(0, |node| node.heuristic_tokens);
    let total = surface.heuristic_surface_tokens();
    Ok(ContextBreakdown {
        system_tokens,
        tools_tokens: estimate_tools(tools),
        message_tokens: total.saturating_sub(system_tokens),
    })
}

/// Prices a request's tool catalogue, the way upstream's `estimateToolsTokens` does.
///
/// The catalogue is serialized and charged at the text density plus one block of structural
/// framing, because a request carries it as one JSON array in a field, not as typed message
/// blocks. An empty catalogue is 0: a request with no tools has no such field, and charging framing
/// for an absent field would overstate every tool-free turn.
///
/// Upstream measures the serialized string's `.length`, which counts UTF-16 code units; this
/// counts `char`s. The two agree on every ASCII tool name and differ by one per astral character
/// in a description — a rate, not a systematic error, and the same density assumption either way.
#[must_use]
pub fn estimate_tools(tools: &[ToolSchema]) -> u64 {
    if tools.is_empty() {
        return 0;
    }
    serde_json::to_string(tools).map_or(0, |json| {
        estimate_text(&json).saturating_add(BLOCK_OVERHEAD)
    })
}

/// Provider-anchored occupancy of the next request's prompt.
///
/// Upstream's `ContextPressureProjection`, including the deliberate non-atomicity: the three fields
/// are last-wins records of *different* moments, so switching models can pair a fresh capacity with
/// the previous route's pressure. That is a display figure, not a gating input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextPressure {
    /// Prompt tokens of the most recent request: what the provider reported as input.
    ///
    /// Absent until a provider reports usage. Response output is excluded, so this does not grow as
    /// a turn streams.
    pub pressure_tokens: Option<u64>,
    /// What the *next* request's prompt would cost: [`Self::pressure_tokens`] plus the heuristic
    /// repricing of everything the surface gained since that sample.
    ///
    /// Only the delta is estimated, so the figure stays anchored to the provider while still
    /// reacting when a compaction shadows a span — which `pressure_tokens` alone cannot do, since a
    /// compaction reports no usage of its own.
    pub projected_tokens: Option<u64>,
    /// Newest recorded route capacity; absent when no event declared one.
    pub context_window: Option<u64>,
}

/// Folds provider pressure and the surface's movement since the sample into one occupancy view.
///
/// The anchor is the point worth stating. A prompt does not contain its own response, so the
/// surface total recorded with a usage sample must *exclude* the response that sample reports.
/// Upstream gets that for free: its usage arrives inside the settlement event and is stamped before
/// that event joins the surface. This crate records the accounting as a separate `model/usage`
/// event immediately *after* the assistant message, so the only link the log carries is adjacency —
/// when the preceding event placed an assistant node, its price is subtracted from the anchor.
/// Without that adjacency the anchor is the plain total, which understates the delta by one
/// response.
///
/// # Errors
///
/// [`ProjectionError::Surface`] when the log is not a valid surface.
pub fn context_pressure(log: &[SurfaceEnvelope]) -> Result<ContextPressure, ProjectionError> {
    fold_surface(log)?;
    let mut builder = SurfaceBuilder::default();
    let mut anchor_exclude: Option<u64> = None;
    let mut pressure_tokens: Option<u64> = None;
    let mut sampled_surface_tokens: Option<u64> = None;
    let mut context_window: Option<u64> = None;

    for envelope in log {
        match &envelope.event {
            SessionEvent::RequestContext {
                context_window: window,
                ..
            } => {
                // Last wins, including a `None` that clears a window the route no longer declares.
                context_window = *window;
            }
            SessionEvent::ModelUsage { input_tokens, .. } => {
                let anchor = builder
                    .surface_tokens
                    .saturating_sub(anchor_exclude.unwrap_or(0));
                pressure_tokens = Some(*input_tokens);
                sampled_surface_tokens = Some(anchor);
            }
            _ => {}
        }
        let placed = builder.apply(envelope)?;
        // Adjacency is the only link the log carries, so it is read strictly: an event that places
        // an assistant node arms the exclusion for the *very next* record, and anything else
        // expires it.
        anchor_exclude = match (placed, &envelope.event) {
            (Some(price), SessionEvent::AssistantMessage { .. }) => Some(price),
            _ => None,
        };
    }

    // Signed, because a compaction can move the surface *below* the sampled total and the
    // difference is a real quantity: clamping it at zero first would silently report a projection
    // that never shrank. The clamp happens on the final figure instead, where a negative prompt is
    // meaningless.
    let projected_tokens =
        pressure_tokens
            .zip(sampled_surface_tokens)
            .map(|(pressure, sampled)| {
                let delta = signed_difference(builder.surface_tokens, sampled);
                let projected = i64::try_from(pressure)
                    .unwrap_or(i64::MAX)
                    .saturating_add(delta);
                u64::try_from(projected.max(0)).unwrap_or(u64::MAX)
            });
    Ok(ContextPressure {
        pressure_tokens,
        projected_tokens,
        context_window,
    })
}

/// Durable usage accumulated across a whole log.
///
/// Upstream's `TokenUsageProjection`, reduced to the buckets this crate's log can carry. The two
/// are disjoint and neither double-counts the other: `input` is what the provider billed as
/// uncached prompt, `output` is completion tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsageProjection {
    /// Prompt tokens the provider billed without a cache hit.
    pub uncached_input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
}

/// Totals provider usage, replacing rather than double-counting a re-reported settlement.
///
/// The rule is upstream's: a usage record for the *same* `(turn, step)` as the previous one is a
/// re-report of the same attempt, so it replaces it; an intervening `llm/retry` closes that slot,
/// so the retried attempt *adds*. That distinction is the whole reason this is a fold and not a
/// `sum`: a retried step legitimately reports twice, and a re-emitted settlement does not.
///
/// Usage with no open step is counted every time it appears. A record outside a step names no
/// attempt, so the log cannot prove two such records are the same one; counting them is the
/// honest reading, and it is the direction that does not hide spend.
///
/// Saturating rather than wrapping, and never failing: a total is a report, and a report that
/// refused because one line was odd would answer nothing at all.
#[must_use]
pub fn fold_token_usage(log: &[SurfaceEnvelope]) -> TokenUsageProjection {
    let mut totals = TokenUsageProjection::default();
    let mut turn: Option<u64> = None;
    let mut step: Option<u64> = None;
    // The (turn, step) the previous record claimed, with its counts, so a re-report can be backed
    // out. Cleared at every boundary and at every retry.
    let mut last: Option<(u64, u64, u64, u64)> = None;

    for envelope in log {
        match &envelope.event {
            SessionEvent::TurnStart { turn: started } => {
                turn = Some(*started);
                step = None;
                last = None;
            }
            SessionEvent::TurnEnd { .. } => {
                turn = None;
                step = None;
                last = None;
            }
            SessionEvent::StepStart { step: started, .. } => {
                step = Some(*started);
                last = None;
            }
            SessionEvent::StepEnd { .. } => {
                step = None;
                last = None;
            }
            SessionEvent::LlmRetry { .. } => last = None,
            SessionEvent::ModelUsage {
                input_tokens,
                output_tokens,
                ..
            } => {
                let key = turn.zip(step);
                let previous = match (key, last) {
                    (Some(current), Some((last_turn, last_step, input, output)))
                        if current == (last_turn, last_step) =>
                    {
                        Some((input, output))
                    }
                    _ => None,
                };
                if let Some((input, output)) = previous {
                    totals.uncached_input_tokens =
                        totals.uncached_input_tokens.saturating_sub(input);
                    totals.output_tokens = totals.output_tokens.saturating_sub(output);
                }
                totals.uncached_input_tokens =
                    totals.uncached_input_tokens.saturating_add(*input_tokens);
                totals.output_tokens = totals.output_tokens.saturating_add(*output_tokens);
                last = key.map(|(turn, step)| (turn, step, *input_tokens, *output_tokens));
            }
            _ => {}
        }
    }
    totals
}

/// Exact provider accounting for one completed turn.
///
/// Upstream's `TurnTokenUsage`, minus the buckets this crate's log cannot carry: the log records no
/// cache-read, cache-write, or reasoning split, so none of them is reported. They are absent rather
/// than zero because a zero would assert that the provider charged nothing for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnTokenUsage {
    /// Sum of uncached prompt input across every billed attempt.
    pub uncached_input_tokens: u64,
    /// Sum of completion tokens across every billed attempt.
    pub output_tokens: u64,
    /// Aggregate prompt plus output across every billed attempt.
    ///
    /// Derived, and equal to the two sums above added: this crate's log records no provider-declared
    /// total, so there is nothing to disagree with. Upstream checks a declared total against the
    /// known prompt precisely because a provider can report one that contradicts its own buckets.
    pub total_tokens: u64,
    /// Models that answered, in first-use order, without duplicates.
    pub models: Vec<String>,
}

impl TurnTokenUsage {
    /// How many billed attempts the turn reported.
    ///
    /// Not stored: derivable figures that a caller can recompute do not belong in a value type, and
    /// keeping them in sync with the sums would be a second thing to get wrong.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.uncached_input_tokens == 0 && self.output_tokens == 0
    }
}

/// Why a turn's exact accounting cannot be proven.
///
/// Upstream answers `undefined` for every one of these. Naming them is a deliberate divergence: a
/// caller showing an accounting panel can say *what* is missing, and a test can assert the specific
/// diagnosis instead of merely that something failed. None of them is an error in the log's
/// content — they are all facts about what the logged lifecycle does and does not settle.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TurnUsageGap {
    /// The slice does not begin with `turn/start`.
    #[error("turn usage: the events do not begin with turn/start")]
    MissingTurnStart,

    /// A second `turn/start` appeared before the first turn ended.
    #[error("turn usage: a second turn/start appears at index {index}")]
    NestedTurnStart {
        /// Position in the slice.
        index: usize,
    },

    /// The turn never closed with `turn/end`.
    #[error("turn usage: the turn never closed with turn/end")]
    MissingTurnEnd,

    /// A `turn/end` named a different turn than the open one.
    #[error("turn usage: turn/end for turn {found} does not close turn {expected}")]
    TurnEndMismatch {
        /// The open turn.
        expected: u64,
        /// The turn the event named.
        found: u64,
    },

    /// Events continued after `turn/end`.
    #[error("turn usage: an event at index {index} follows turn/end")]
    TrailingEvent {
        /// Position in the slice.
        index: usize,
    },

    /// A step event named a different turn than the open one.
    #[error("turn usage: {event} names turn {found} while turn {expected} is open")]
    StepTurnMismatch {
        /// The event's type tag.
        event: &'static str,
        /// The open turn.
        expected: u64,
        /// The turn the event named.
        found: u64,
    },

    /// A `step/start` arrived while a step was still open.
    #[error("turn usage: step/start at index {index} arrived before the open step ended")]
    NestedStepStart {
        /// Position in the slice.
        index: usize,
    },

    /// A `step/end` did not match the open step.
    #[error("turn usage: step/end at index {index} does not match the open step")]
    UnmatchedStepEnd {
        /// Position in the slice.
        index: usize,
    },

    /// The open step never closed with `step/end`.
    #[error("turn usage: the open step never closed with step/end")]
    UnclosedStep,

    /// A `model/usage` record arrived with no open step.
    ///
    /// Every billed attempt belongs to a step; a record outside one has no attempt identity, so it
    /// cannot be told apart from a duplicate of another.
    #[error("turn usage: model/usage at index {index} arrived outside an open step")]
    UsageOutsideStep {
        /// Position in the slice.
        index: usize,
    },

    /// The turn reported no usage at all.
    ///
    /// Distinct from "used nothing": a turn with no record may have been billed, and reporting
    /// zero would claim it was not.
    #[error("turn usage: no model/usage record settled an attempt")]
    NoBillingAttempt,

    /// Summing the attempts overflowed a 64-bit token count.
    #[error("turn usage: summing attempts overflowed a 64-bit token count")]
    Overflow,
}

/// Folds one complete turn's durable lifecycle into exact token accounting.
///
/// `log` must be the turn-local slice from `turn/start` through its `turn/end`. Anything less is
/// refused: a missing boundary, a nested step, an unclosed step, or a usage record outside a step
/// all make the total unprovable, and a plausible-looking wrong total is worse than a named gap.
///
/// Retried attempts add. A `model/usage` record for the same `(turn, step)` as the previous one
/// *replaces* it — that is a re-emitted settlement, not a second call — and a `llm/retry` between
/// them reopens the slot, because the retried attempt is a different call the provider billed.
///
/// # Errors
///
/// [`TurnUsageGap`], naming the first thing that made the total unprovable.
#[allow(clippy::too_many_lines)] // One match over the eight lifecycle events; splitting it would
// scatter one state machine across helpers and hide which transitions close which slot.
pub fn derive_turn_usage(log: &[SurfaceEnvelope]) -> Result<TurnTokenUsage, TurnUsageGap> {
    let mut turn: Option<u64> = None;
    let mut open_step: Option<u64> = None;
    let mut closed = false;
    let mut attempts: Vec<Attempt> = Vec::new();
    // The attempt the next record for the same key may replace, with its index.
    let mut slot: Option<((u64, u64), usize)> = None;

    for (index, envelope) in log.iter().enumerate() {
        if closed {
            return Err(TurnUsageGap::TrailingEvent { index });
        }
        match &envelope.event {
            SessionEvent::TurnStart { turn: started } => {
                if turn.is_some() {
                    return Err(TurnUsageGap::NestedTurnStart { index });
                }
                turn = Some(*started);
            }
            SessionEvent::TurnEnd { turn: ended, .. } => {
                let Some(expected) = turn else {
                    return Err(TurnUsageGap::MissingTurnStart);
                };
                if *ended != expected {
                    return Err(TurnUsageGap::TurnEndMismatch {
                        expected,
                        found: *ended,
                    });
                }
                if open_step.is_some() {
                    return Err(TurnUsageGap::UnclosedStep);
                }
                closed = true;
            }
            SessionEvent::StepStart {
                turn: named, step, ..
            } => {
                let Some(expected) = turn else {
                    return Err(TurnUsageGap::MissingTurnStart);
                };
                if *named != expected {
                    return Err(TurnUsageGap::StepTurnMismatch {
                        event: envelope.event.type_name(),
                        expected,
                        found: *named,
                    });
                }
                if open_step.is_some() {
                    return Err(TurnUsageGap::NestedStepStart { index });
                }
                open_step = Some(*step);
                slot = None;
            }
            SessionEvent::StepEnd {
                turn: named, step, ..
            } => {
                let Some(expected) = turn else {
                    return Err(TurnUsageGap::MissingTurnStart);
                };
                if *named != expected {
                    return Err(TurnUsageGap::StepTurnMismatch {
                        event: envelope.event.type_name(),
                        expected,
                        found: *named,
                    });
                }
                if open_step != Some(*step) {
                    return Err(TurnUsageGap::UnmatchedStepEnd { index });
                }
                open_step = None;
                slot = None;
            }
            SessionEvent::LlmRetry { .. } => slot = None,
            SessionEvent::ModelUsage {
                model,
                input_tokens,
                output_tokens,
            } => {
                let Some(step) = open_step else {
                    return Err(TurnUsageGap::UsageOutsideStep { index });
                };
                let Some(expected) = turn else {
                    return Err(TurnUsageGap::MissingTurnStart);
                };
                let attempt = Attempt {
                    model: model.clone(),
                    input: *input_tokens,
                    output: *output_tokens,
                };
                let key = (expected, step);
                match slot {
                    Some((claimed, at)) if claimed == key => attempts[at] = attempt,
                    _ => {
                        attempts.push(attempt);
                        slot = Some((key, attempts.len().saturating_sub(1)));
                    }
                }
            }
            _ => {}
        }
    }

    if !closed {
        return Err(TurnUsageGap::MissingTurnEnd);
    }
    if attempts.is_empty() {
        return Err(TurnUsageGap::NoBillingAttempt);
    }
    aggregate(&attempts)
}

/// Accumulates one attempt.
///
/// A private struct rather than a tuple because three same-typed `u64`s in a row is the kind of
/// shape where a transposed field is invisible at the call site.
struct Attempt {
    model: String,
    input: u64,
    output: u64,
}

/// Sums the attempts into the disclosure, refusing an unsafe total.
fn aggregate(attempts: &[Attempt]) -> Result<TurnTokenUsage, TurnUsageGap> {
    let mut uncached_input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut models: Vec<String> = Vec::new();
    for attempt in attempts {
        uncached_input_tokens = uncached_input_tokens
            .checked_add(attempt.input)
            .ok_or(TurnUsageGap::Overflow)?;
        output_tokens = output_tokens
            .checked_add(attempt.output)
            .ok_or(TurnUsageGap::Overflow)?;
        if !models.contains(&attempt.model) {
            models.push(attempt.model.clone());
        }
    }
    let total_tokens = uncached_input_tokens
        .checked_add(output_tokens)
        .ok_or(TurnUsageGap::Overflow)?;
    Ok(TurnTokenUsage {
        uncached_input_tokens,
        output_tokens,
        total_tokens,
        models,
    })
}

/// One turn found in a log, with whatever accounting could be proven for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnUsageEntry {
    /// The turn number the slice opened with.
    pub turn: u64,
    /// Its accounting, or the reason there is none.
    pub usage: Result<TurnTokenUsage, TurnUsageGap>,
}

/// Splits a log at its turn boundaries and folds each turn separately.
///
/// A turn that never closed is folded from its `turn/start` to the end of the slice, so it reports
/// [`TurnUsageGap::MissingTurnEnd`] rather than vanishing: a crash mid-turn left real spend behind,
/// and omitting the entry would make the log look shorter than it was.
///
/// Events before the first `turn/start` are skipped. They have no turn to belong to, and inventing
/// one would attribute session-level records to a conversation the model never had.
#[must_use]
pub fn derive_turn_usages(log: &[SurfaceEnvelope]) -> Vec<TurnUsageEntry> {
    let mut entries = Vec::new();
    let mut open: Option<(u64, usize)> = None;
    for (index, envelope) in log.iter().enumerate() {
        match &envelope.event {
            SessionEvent::TurnStart { turn } => {
                if let Some((previous, from)) = open.replace((*turn, index)) {
                    entries.push(TurnUsageEntry {
                        turn: previous,
                        usage: derive_turn_usage(&log[from..index]),
                    });
                }
            }
            SessionEvent::TurnEnd { .. } => {
                if let Some((turn, from)) = open.take() {
                    entries.push(TurnUsageEntry {
                        turn,
                        usage: derive_turn_usage(&log[from..=index]),
                    });
                }
            }
            _ => {}
        }
    }
    if let Some((turn, from)) = open {
        entries.push(TurnUsageEntry {
            turn,
            usage: derive_turn_usage(&log[from..]),
        });
    }
    entries
}

/// Mutable priced-surface state, shared by the folds that need running totals.
///
/// Private because it is a fold's *implementation*, not a shape a caller should build by hand: the
/// public entry points hand back finished values, which is what makes them reproducible.
#[derive(Debug, Default)]
struct SurfaceBuilder {
    nodes: Vec<PricedNode>,
    transitions: Vec<SurfaceTransition>,
    surface_tokens: u64,
}

impl SurfaceBuilder {
    /// Applies one envelope, returning the price of the node it placed when it placed one.
    ///
    /// `Ok(None)` means the event is log-only: it has no placement, so it changed no price. The
    /// distinction matters to [`context_pressure`], which uses the returned price to decide whether
    /// the *next* usage record settles a response.
    fn apply(&mut self, envelope: &SurfaceEnvelope) -> Result<Option<u64>, ProjectionError> {
        let Some(op) = envelope.op else {
            return Ok(None);
        };
        let node = priced_node(envelope);
        let tokens = node.heuristic_tokens;
        let (target, removed) = match op {
            SurfaceOp::Append => (SurfaceTarget::Append, 0),
            SurfaceOp::Replace { start, end } => {
                let (start_idx, end_idx) = locate(&self.nodes, envelope.seq, start, end)?;
                let removed = self.nodes[start_idx..=end_idx]
                    .iter()
                    .fold(0u64, |total, node| {
                        total.saturating_add(node.heuristic_tokens)
                    });
                (SurfaceTarget::Replace { start_idx, end_idx }, removed)
            }
        };
        self.transitions.push(SurfaceTransition {
            seq: envelope.seq,
            tokens,
            delta_tokens: signed_difference(tokens, removed),
            target,
        });
        match target {
            SurfaceTarget::Append => {
                self.surface_tokens = self.surface_tokens.saturating_add(tokens);
                self.nodes.push(node);
            }
            SurfaceTarget::Replace { start_idx, end_idx } => {
                self.surface_tokens = self
                    .surface_tokens
                    .saturating_sub(removed)
                    .saturating_add(tokens);
                self.nodes.splice(start_idx..=end_idx, [node]);
            }
        }
        Ok(Some(tokens))
    }
}

/// Prices one surface event's node under the fixed heuristic.
///
/// A node is placed even when it derives no message. Upstream does the same, and the reason is that
/// an empty assistant settlement still belongs on the surface: dropping the node would make a
/// replacement range naming it unresolvable.
fn priced_node(envelope: &SurfaceEnvelope) -> PricedNode {
    let mut images = Vec::new();
    let heuristic_tokens = if let Some(message) = derive_event_message(envelope) {
        collect_images(&message.content, &mut images);
        price_message(message)
    } else if let Some(content) = tool_result_content(envelope) {
        collect_images(content, &mut images);
        price_blocks(content)
    } else {
        // An event that derives no message still occupies its surface position: a replacement
        // naming it must resolve, so it is priced as a node worth zero rather than omitted.
        0
    };
    PricedNode {
        seq: envelope.seq,
        heuristic_tokens,
        tokens: heuristic_tokens,
        is_system: matches!(envelope.event, SessionEvent::SystemMessage { .. }),
        images,
    }
}

/// Prices one logged message, including its role framing.
///
/// The system arm is upstream's `estimateSystemMessage`, and it is a different *shape* rather than a
/// different constant: a system prompt is serialized as one plain string — a system-role message or
/// the request's `system` field — not as typed content blocks, so it is charged text density plus
/// role framing and no per-block overhead. Charging block overhead for each block of a prompt would
/// overstate every step, because the prompt is re-sent unchanged.
///
/// This is why [`girder_core::estimate_message`] is not reused: that function takes an
/// `llm::Message`, whose role enum has no system arm, precisely because a request carries the prompt
/// out of band. A log *does* hold system-role messages ([`ChatMessage`]), so the arm is reachable
/// here and would otherwise be silently mispriced.
///
/// An empty system message is 0, not framing: "no prompt" and "an empty prompt" are the same request
/// field, and upstream returns 0 for exactly that reason.
fn price_message(message: &ChatMessage) -> u64 {
    if message.role == ChatRole::System {
        return price_system_message(message);
    }
    price_blocks(&message.content)
}

/// Prices the rendered system prompt: its text density plus role framing, with no block overhead.
///
/// Non-text blocks are serialized and charged by their JSON length, as upstream does. The JSON is
/// this crate's shape rather than upstream's, so a prompt carrying a non-text block is priced by a
/// proxy — the same kind of proxy the fixed heuristic is, and the same direction of error.
fn price_system_message(message: &ChatMessage) -> u64 {
    if message.content.is_empty() {
        return 0;
    }
    let mut rendered = String::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => rendered.push_str(text),
            // A serialization failure cannot happen for these types; contributing nothing is the
            // only honest fallback, and it can only underprice a prompt that has no text at all.
            other => {
                if let Ok(json) = serde_json::to_string(other) {
                    rendered.push_str(&json);
                }
            }
        }
    }
    estimate_text(&rendered).saturating_add(ROLE_OVERHEAD)
}

/// Prices content blocks plus one message's framing.
fn price_blocks(blocks: &[ContentBlock]) -> u64 {
    estimate_blocks(blocks).saturating_add(ROLE_OVERHEAD)
}

/// Collects the image occurrences of one message, in message order.
///
/// Only images: a file's request representation comes from the mounted LLM service, which this
/// crate does not have, so a file keeps its structural price (see the module documentation).
fn collect_images(blocks: &[ContentBlock], out: &mut Vec<ImageOccurrence>) {
    for block in blocks {
        if let ContentBlock::Image { attachment } = block {
            out.push(ImageOccurrence {
                reference: attachment.clone(),
                structural_tokens: estimate_blocks(std::slice::from_ref(block)),
            });
        }
    }
}

/// Finds a replacement range on the current node list.
///
/// [`girder_core::fold_surface`] has already validated the log, so a miss here is unreachable; it
/// returns the same [`SurfaceError`] the fold would rather than panicking, because a panic in a
/// price fold would take down a session view over a number.
fn locate(
    nodes: &[PricedNode],
    seq: u64,
    start: u64,
    end: u64,
) -> Result<(usize, usize), ProjectionError> {
    let find = |wanted: u64| nodes.iter().position(|node| node.seq == wanted);
    let (Some(start_idx), Some(end_idx)) = (find(start), find(end)) else {
        return Err(ProjectionError::Surface(SurfaceError::InvalidRange {
            seq,
            detail: format!("replacement range {start}-{end} is not on the current surface"),
        }));
    };
    if start_idx > end_idx {
        return Err(ProjectionError::Surface(SurfaceError::InvalidRange {
            seq,
            detail: format!(
                "replacement start seq {start} (index {start_idx}) is after end seq {end} (index {end_idx})"
            ),
        }));
    }
    Ok((start_idx, end_idx))
}

/// The signed difference `tokens - removed`, saturating instead of wrapping.
fn signed_difference(tokens: u64, removed: u64) -> i64 {
    let tokens = i64::try_from(tokens).unwrap_or(i64::MAX);
    let removed = i64::try_from(removed).unwrap_or(i64::MAX);
    tokens.saturating_sub(removed)
}
