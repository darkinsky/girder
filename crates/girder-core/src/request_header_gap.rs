//! Request-header reconstruction: canonical form, equality, and the fold that answers "what request
//! is in force right now".
//!
//! # What upstream has, measured
//!
//! `packages/core/session/src/request-header.ts` is 69 lines with exactly three exports:
//!
//! | Upstream | Lines | What it decides |
//! | --- | --- | --- |
//! | `canonicalHeader` | 10 | which optional fields *exist*: an empty tool list and a default-less adapter record become absent |
//! | `headerEquals` | 21 | whether a proposed header is a real change (worth a log record) or the held one restated |
//! | `foldRequestHeader` | 7 | the header in force after the last `request/header` snapshot in a log prefix |
//!
//! Small, and load-bearing. The `EpochHeader` it operates on is *the* description of a request's
//! envelope: the conversation's call configuration (provider, model, reasoning effort, temperature,
//! `max_tokens`, `stop`), the fields an adapter materialized rather than a caller proposed, and the
//! assembled tool schemas. Upstream's `agent-loop` uses these three functions for two decisions in
//! `agent.ts`:
//!
//! - whether a step must append a `request/header` record at all — an unchanged header must *not*
//!   be logged, or every step would append noise and the log would stop explaining anything
//!   (`baseline === undefined || !headerEquals(baseline, header)`);
//! - whether the assembled tools differ from the logged ones (`toolsChanged`), which is what
//!   turns a tool addition into a `developer/message` rather than a silent change of the tool set
//!   the model sees mid-conversation.
//!
//! And the fold is the offline half of the same contract: anyone holding a session log reconstructs
//! the request envelope of any prefix by taking the latest canonical snapshot. That is what makes
//! "every request the loop sends is a pure function of the log" testable rather than aspirational
//! (`packages/core/agent-loop/tests/request-reconstruction.spec.ts`).
//!
//! Two properties are easy to miss and are the reason this module exists as one unit:
//!
//! 1. **Canonical form is about absence, not emptiness.** An empty `tools` array and a missing
//!    `tools` field mean the same thing to a request builder (a tool-less request), so the header
//!    has one representation: absent. If they were both representable, the same request would log
//!    differently depending on which code path built it, and `headerEquals` would report a change
//!    on every step.
//! 2. **Equality is field-wise, and the tool list is ordered.** Tool order is part of what the model
//!    sees, so two headers with the same tools in a different order are different headers — they
//!    are a different request, and a cache or a replay that treats them as equal would be wrong.
//!
//! # What this crate already covers, and what the gap was
//!
//! Two local types look like the answer and are not:
//!
//! - [`girder_core::SessionEvent::RequestContext`] (`request/context`) records `provider`, `model`
//!   and `context_window`. It is *route capacity*, not the request envelope: upstream's own type
//!   documentation for the same record says it "does not participate in request reconstruction or
//!   header equality". It cannot answer "which tools did this request offer", which is the whole
//!   point of the header.
//! - [`girder_core::CallConfig`] (`call_config`) holds `max_tokens`, `temperature`, `timeout`,
//!   `first_token_timeout` and `retries` — per-call *deadlines and retry budget*, resolved from a
//!   pattern table. Two of its fields overlap the header's sampling scalars, but it has no
//!   provider, no model, no reasoning effort, no `stop`, no adapter record and no tools; it is a
//!   policy a request is built under, not a description of the request.
//!
//! `SessionEvent` has **no `request/header` variant at all**, so a local log cannot record a request
//! envelope and a local replay cannot reconstruct one. `core_gap.rs` names this gap explicitly
//! ("No `request/header` fold ... inventing one here would create a second source of truth for tool
//! declarations"). The vocabulary half of that objection is now answerable:
//! [`girder_core::ToolSchema`] exists in `llm` and is exactly the shape upstream logs. What is
//! still true — and what shapes this module — is that no *event variant* carries it.
//!
//! # The gap, precisely
//!
//! | Concern | State |
//! | --- | --- |
//! | `EpochHeader` (config + adapter defaults + ordered tools) | absent; defined here |
//! | canonical form (empty tools / default-less adapter record → absent) | absent; [`canonical_header`] |
//! | field-wise equality incl. ordered tool schemas | absent; [`header_equals`] |
//! | fold a log prefix to the header in force | absent; [`fold_request_header`] |
//! | incremental fold for a live session | absent; [`HeaderFold`] |
//! | `request/header` **event variant** and its `seq`/`time` envelope | **not here** — belongs in `session`, and adding it means touching `session.rs`, which this change does not |
//! | deciding *which* `reason` to record, and `startsSeries` vs surface generation | **not here** — that is the loop's decision (`agent.ts`), and it reads the fold's answer rather than inventing one |
//! | logging the tool add/remove `developer/message` diff | **not here** — same reason; it consumes [`header_equals`] |
//! | `header.system?: never` (retired request text) | **deliberately not modelled** — a field that may only be absent is not representable in Rust, and v2→v3 migration already drops it (`session_migration`) |
//!
//! # What this module therefore is
//!
//! The three upstream functions, plus the vocabulary they operate on, as a pure library over a
//! slice: no I/O, no session ownership, no clock. The fold takes a *mixed* event slice — real
//! [`girder_core::SessionEvent`]s interleaved with [`HeaderSnapshot`]s — rather than a list of
//! headers, because "non-header events are skipped" is a property that must be exercised against
//! something that could be mistaken for a header. When `session` grows the variant, the only change
//! here is one more `HeaderEvent` constructor.
//!
//! # Two deliberate differences from upstream, and why
//!
//! 1. **Tool schemas are compared by [`girder_core::canonical_json`], not by `JSON.stringify`.**
//!    Upstream stringifies, which is sensitive to object key *insertion order*; its own comment
//!    concedes the comparison is only valid for schemas "assembled through the same path". The
//!    crate's canonical encoder sorts keys at every depth instead of trusting the map backend, so
//!    two spellings of one schema compare equal here and unequal upstream. That is the harmless
//!    direction (a missed "changed" is a missing log record, not a wrong one) and it removes a
//!    dependency on how `serde_json` was compiled. Numbers are still not normalized: `1` and `1.0`
//!    encode differently, exactly as `JSON.stringify` keeps `1` and `1` distinct from `1.0`.
//! 2. **Sampling temperature compares by IEEE equality, NaN included.** Upstream uses JavaScript's
//!    `!==`, under which `NaN !== NaN` (a re-proposed NaN is always a change) and `-0.0 === 0.0`
//!    (the sign of zero is not a change). [`header_equals`] reproduces both.
//!
//! A third difference is a consequence of the gap rather than a choice: `max_tokens` here is
//! `u32`, because [`girder_core::LlmRequest::max_tokens`] is `Option<u32>` and the header records
//! *that* request. A wider field would let the two disagree, and the header would promise a limit
//! the request cannot carry.

// This module is compiled by `tests/request_header_gap.rs` through `#[path]` before `lib.rs`
// declares it, so "unused within this test binary" is not evidence of dead code: every item is
// public surface for the wiring that follows.
#![allow(
    dead_code,
    reason = "compiled privately by the `#[path]` test until lib.rs re-exports it"
)]

use girder_core::{SessionEvent, ToolSchema, canonical_json};
use serde::{Deserialize, Serialize};

/// The conversation's call configuration: the fields a request is built from, per request.
///
/// Field order is the canonical order — the order this struct serializes in, and the order
/// [`canonical_header`] promises. Upstream's `LlmCallConfig` has the same six fields; `provider`
/// and `model` are required there and required here, because a header without a route is not a
/// request anybody could dispatch, and making the halves optional would let a log hold one of them.
///
/// Every optional field is *omitted* from the wire when absent rather than written as `null`,
/// matching how the request is built: a caller that said nothing about `temperature` must not be
/// recorded as having chosen `null`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderConfig {
    /// The registered provider route.
    pub provider: String,
    /// The provider-owned model id.
    pub model: String,
    /// The adapter-owned reasoning effort, when one was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Sampling temperature, when the caller proposed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// The output-token cap this request carried, when it carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Stop sequences, when the caller proposed any.
    ///
    /// Absent and empty are *different* here, unlike the tool list: upstream's `callConfigEquals`
    /// treats a missing `stop` and `stop: []` as different configurations, because `[]` is an
    /// explicit statement ("no stop sequences") and absence is silence. Normalizing them would make
    /// the log unable to distinguish a caller that cleared the list from one that never set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

impl HeaderConfig {
    /// Builds a configuration naming a route, with every optional field unset.
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
            stop: None,
        }
    }

    /// Sets the reasoning effort.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// Sets the sampling temperature.
    #[must_use]
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Sets the output-token cap.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets the stop sequences.
    #[must_use]
    pub fn with_stop(mut self, stop: Vec<String>) -> Self {
        self.stop = Some(stop);
        self
    }
}

/// Config fields an exact-model adapter materialized rather than a caller proposing them.
///
/// Upstream types both flags as the literal `true`, so the only information a present flag carries
/// is *presence*. Modelling them as `bool` is the same set of states — and it is what lets
/// [`canonical_header`] delete a record whose flags are all `false`, which the upstream literal
/// cannot express and therefore never has to.
///
/// The distinction is not cosmetic: on resume, a header whose `maxTokens` came from the adapter
/// must not be mistaken for one where the user asked for that number, or the UI would offer to
/// "restore" a setting nobody set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterDefaults {
    /// Whether the reasoning effort came from the adapter.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reasoning_effort: bool,
    /// Whether the token cap came from the adapter.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub max_tokens: bool,
}

impl AdapterDefaults {
    /// A record in which nothing came from the adapter.
    ///
    /// Legal to construct and equal to absence by [`canonical_header`]'s rule; kept because a
    /// caller that builds the record from two booleans should not need an `if`.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            reasoning_effort: false,
            max_tokens: false,
        }
    }

    /// Whether no field came from the adapter, so the record is canonically absent.
    ///
    /// Takes `self` by value rather than by reference because the record is two flags: a reference
    /// would cost more than the copy, and the method is meant to be called exactly that cheaply.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.reasoning_effort && !self.max_tokens
    }
}

/// The full envelope one request is built under: the `EpochHeader` of upstream.
///
/// "Epoch" because it is stable across steps until something changes it: every step in a turn (and
/// every turn) reuses the same header unless the model selection, the sampling scalars, or the tool
/// set moved. That is what makes an unchanged header worth *not* logging.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochHeader {
    /// The conversation's call configuration.
    pub config: HeaderConfig,
    /// Fields the adapter materialized, when any did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_defaults: Option<AdapterDefaults>,
    /// The assembled tool schemas, in the order the model sees them.
    ///
    /// `Option` rather than `Vec` so that a tool-less request has exactly one representation after
    /// [`canonical_header`]; see the module documentation. Order is meaningful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSchema>>,
}

impl EpochHeader {
    /// Builds a header with no adapter record and no tools.
    #[must_use]
    pub fn new(config: HeaderConfig) -> Self {
        Self {
            config,
            adapter_defaults: None,
            tools: None,
        }
    }

    /// Sets the adapter record.
    #[must_use]
    pub fn with_adapter_defaults(mut self, adapter_defaults: AdapterDefaults) -> Self {
        self.adapter_defaults = Some(adapter_defaults);
        self
    }

    /// Sets the tool schemas.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// The tools the model is offered, with canonical absence flattened to an empty slice.
    ///
    /// The read side of the rule [`canonical_header`] writes: a consumer that walks the tool list
    /// must not have to spell `as_deref().unwrap_or_default()` and must not be able to panic on a
    /// non-canonical header.
    #[must_use]
    pub fn tool_slice(&self) -> &[ToolSchema] {
        self.tools.as_deref().unwrap_or_default()
    }
}

/// Why a `request/header` snapshot was appended.
///
/// Recorded but **inert to the fold**: the fold's answer is the latest snapshot's header whatever
/// the reason, because the reason explains the log to a human reader, not the request to a builder.
/// It is modelled anyway, because a local event that could not carry it would lose the one piece of
/// information that distinguishes "the header changed" from "the header was restated to open a new
/// message series".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeaderReason {
    /// The log's first header: a new conversation.
    Initial,
    /// A loop instance's first request over a log that already has header events.
    Resume,
    /// A later request used a different header.
    Change,
    /// An unchanged header began an explicitly distinct message series.
    Series,
}

/// One `request/header` record: the header, why it was written, and whether it opens a series.
///
/// The sequence/time envelope is not here: this is the *payload*, and the envelope belongs to the
/// session log that stores it (see the module documentation's gap table).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderSnapshot {
    /// The header in force for the next request.
    pub header: EpochHeader,
    /// Why the snapshot was appended.
    pub reason: HeaderReason,
    /// Whether this request begins a distinct model-message series, independently of the reason.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub starts_series: bool,
}

impl HeaderSnapshot {
    /// Builds a snapshot that does not open a series.
    #[must_use]
    pub fn new(header: EpochHeader, reason: HeaderReason) -> Self {
        Self {
            header,
            reason,
            starts_series: false,
        }
    }

    /// Marks the snapshot as opening a new message series.
    #[must_use]
    pub const fn starting_series(mut self) -> Self {
        self.starts_series = true;
        self
    }
}

/// One entry of a mixed log: something recorded, and (sometimes) a header.
///
/// The fold needs to see the *other* events, not just the header ones. Folding a list that contains
/// only snapshots would pass even if the implementation stopped on the first event, which is
/// exactly the bug a real log would expose — a `request/context` between two headers, or a user
/// message after the last header, must leave the answer alone.
#[derive(Clone, Copy, Debug)]
pub enum HeaderEvent<'a> {
    /// Any event with no bearing on the request envelope.
    Other(&'a SessionEvent),
    /// A `request/header` snapshot.
    Snapshot(&'a HeaderSnapshot),
}

impl<'a> From<&'a SessionEvent> for HeaderEvent<'a> {
    fn from(event: &'a SessionEvent) -> Self {
        Self::Other(event)
    }
}

impl<'a> From<&'a HeaderSnapshot> for HeaderEvent<'a> {
    fn from(snapshot: &'a HeaderSnapshot) -> Self {
        Self::Snapshot(snapshot)
    }
}

/// Normalizes a header to canonical form.
///
/// Three rules, and they are the whole of what "canonical" means here:
///
/// 1. **Field order is fixed.** The order is [`EpochHeader`]'s declaration order — `config`,
///    `adapterDefaults`, `tools` — which is also its serialization order. A caller cannot reorder
///    it, so two headers that agree on values agree on their encoding.
/// 2. **Empty optional fields become absent.** An empty tool list means a tool-less request, and a
///    tool-less request has no `tools` field; an adapter record with no `true` flag carries no
///    information and is dropped. Without this rule the same request logs differently depending on
///    which code path assembled it, and [`header_equals`] reports a change on every step.
/// 3. **Omitted optional fields stay omitted.** `temperature: None` remains absent rather than
///    being filled with the provider's default. Filling it here would duplicate a default that
///    belongs to the provider and would make the log claim a value the caller never chose.
///
/// The function is idempotent: [`canonical_header`] of a canonical header is that header.
#[must_use]
pub fn canonical_header(header: &EpochHeader) -> EpochHeader {
    EpochHeader {
        config: header.config.clone(),
        // Rule 2, second half: a record whose flags are all false is the same as no record.
        adapter_defaults: header
            .adapter_defaults
            .filter(|adapter_defaults| !adapter_defaults.is_empty()),
        // Rule 2, first half: an empty tool list is a tool-less request, which has no field.
        tools: header.tools.clone().filter(|tools| !tools.is_empty()),
    }
}

/// Whether two headers describe the same request.
///
/// Field-wise over the canonical fields, so callers do not have to canonicalize first: an absent
/// tool list and an empty one are equal here, because they are the same request. Every one of the
/// six config scalars is compared — a change in `temperature` or `stop` is as much a different
/// request as a change of model, and a loop that only compared provider and model would silently
/// refuse to log a sampling change.
///
/// The tool list compares **in order**: the order is what the model sees, so two headers with the
/// same tools in a different order are different headers.
#[must_use]
pub fn header_equals(left: &EpochHeader, right: &EpochHeader) -> bool {
    config_equals(&left.config, &right.config)
        && adapter_flag(left.adapter_defaults.as_ref())
            == adapter_flag(right.adapter_defaults.as_ref())
        && tools_equal(left.tool_slice(), right.tool_slice())
}

/// Folds a mixed log prefix into the header in force after its last snapshot.
///
/// `from` continues a previous fold, so the result of folding `a` then `b` is the result of
/// folding `a ++ b`. When no snapshot follows, `from` is returned unchanged — the caller's own
/// value, not a canonicalized copy — because "nothing happened" must not quietly rewrite the
/// baseline a live session is holding.
///
/// The returned header is always canonical, even when the snapshot it came from was not: a log
/// written before the canonicalization rule existed must still fold to a usable header. Upstream
/// gets that for free by canonicalizing at write time; doing it at read time too means a
/// hand-written or older record cannot poison the answer.
#[must_use]
pub fn fold_request_header(
    events: &[HeaderEvent<'_>],
    from: Option<&EpochHeader>,
) -> Option<EpochHeader> {
    let mut state = from.cloned();
    for event in events {
        if let HeaderEvent::Snapshot(snapshot) = event {
            state = Some(canonical_header(&snapshot.header));
        }
    }
    state
}

/// The incremental form of [`fold_request_header`], for a session that is still being written.
///
/// Upstream notes that "the live session tracks the same fold incrementally": a loop that has
/// already seen the log should not re-read it to answer `requestHeader()`. This type is that
/// watermark — one snapshot at a time, with no slice to keep alive.
///
/// It agrees with the batch fold by construction, and the tests assert it rather than assuming it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeaderFold {
    /// The header in force, in canonical form.
    state: Option<EpochHeader>,
}

impl HeaderFold {
    /// Starts a fold from a previously folded state.
    #[must_use]
    pub fn new(from: Option<EpochHeader>) -> Self {
        Self { state: from }
    }

    /// The header in force, or `None` before any snapshot.
    #[must_use]
    pub fn state(&self) -> Option<&EpochHeader> {
        self.state.as_ref()
    }

    /// Takes ownership of the folded header.
    #[must_use]
    pub fn into_state(self) -> Option<EpochHeader> {
        self.state
    }

    /// Whether a header has been observed yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.state.is_none()
    }

    /// Folds one event. Non-snapshot events are ignored.
    pub fn observe(&mut self, event: HeaderEvent<'_>) {
        if let HeaderEvent::Snapshot(snapshot) = event {
            self.state = Some(canonical_header(&snapshot.header));
        }
    }

    /// Folds every event of a batch.
    pub fn observe_all(&mut self, events: &[HeaderEvent<'_>]) {
        for event in events {
            self.observe(*event);
        }
    }
}

/// The config half of [`header_equals`]. Upstream's `callConfigEquals`, field for field.
fn config_equals(left: &HeaderConfig, right: &HeaderConfig) -> bool {
    left.provider == right.provider
        && left.model == right.model
        && left.reasoning_effort == right.reasoning_effort
        && same_temperature(left.temperature, right.temperature)
        && left.max_tokens == right.max_tokens
        && same_stop(left.stop.as_deref(), right.stop.as_deref())
}

/// Whether two optional temperatures are the same configuration.
///
/// Deliberately IEEE equality rather than a bit-pattern or epsilon comparison, because that is what
/// a caller in JavaScript observed: `NaN !== NaN`, so a re-proposed `NaN` is always a change (a NaN
/// temperature cannot be dispatched, and reporting it as unchanged would keep the request alive
/// with a value that will fail); `-0.0 === 0.0`, so the sign of zero is not a change. Upstream's
/// `!==` cannot compare `-0.0` against `0.0` as different, and neither does this.
#[allow(
    clippy::float_cmp,
    reason = "IEEE equality is the specified behaviour: NaN is always a change, -0.0 equals 0.0"
)]
fn same_temperature(left: Option<f32>, right: Option<f32>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Whether two optional stop lists are the same configuration, element-wise.
///
/// Absent and empty differ: see [`HeaderConfig::stop`]. Within a present list only the elements and
/// their order matter, which slice equality already states.
fn same_stop(left: Option<&[String]>, right: Option<&[String]>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left == right,
        // One absent and one present: `None` means "said nothing", `Some` means "said something".
        (None, Some(_)) | (Some(_), None) => false,
    }
}

/// The adapter flags as one comparable pair, with absence flattened to "nothing came from the
/// adapter".
fn adapter_flag(adapter_defaults: Option<&AdapterDefaults>) -> (bool, bool) {
    adapter_defaults.map_or((false, false), |record| {
        (record.reasoning_effort, record.max_tokens)
    })
}

/// Whether two tool lists describe the same offer, in order.
fn tools_equal(left: &[ToolSchema], right: &[ToolSchema]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| same_schema(left, right))
}

/// Whether two tool schemas are the same schema.
///
/// `canonical_json` rather than `==` on the two `Value`s: the parameters are a JSON Schema object,
/// JSON object members are unordered by the specification, and `serde_json::Map` is sorted only
/// under its default features. The crate's encoder sorts at every depth explicitly, so this
/// comparison does not change meaning if a dependency enables `preserve_order`.
///
/// `name` and `description` are compared as the strings they are — they are `String` fields, not
/// JSON, and encoding them would only add work.
fn same_schema(left: &ToolSchema, right: &ToolSchema) -> bool {
    left.name == right.name
        && left.description == right.description
        && canonical_json(&left.parameters) == canonical_json(&right.parameters)
}
