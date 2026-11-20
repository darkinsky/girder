//! The durable subagent descriptor, and the identity and timing projections folded from it.
//!
//! # What the measurement said
//!
//! Round two of the "compare upstream error messages and constant tables" sweep covered a different
//! slice of upstream than round one (`hook-protocol`): `packages/{subagent,subprocess,fs,shell,sandbox}`.
//! Angle A (error strings) and angle B (`const` names) together produced twelve concrete leads. Ten
//! turned out to have a local equivalent already — the sandbox ACL family in [`crate::sandbox::policy`],
//! `/proc` process inspection in [`crate::shell::subprocess_deep`], literal-argv handling in
//! [`crate::sandbox::policy`], the unattended-approval policy in [`crate::subagent::deep`], `DENIAL_SIGNATURES`
//! in [`crate::sandbox::policy`], `STRUCTURED_OUTPUT_TOOL` in [`crate::subagent::extra`], and
//! `SandboxInvalidRootError` in [`crate::sandbox::policy`]. Two were genuine, and this module ports the
//! larger one.
//!
//! The larger gap is upstream `packages/subagent/subagent/src/descriptor.ts` (173 effective lines)
//! together with the two session projections that read it, `projection.ts` (129 effective lines) and
//! `archive-admission.ts` (54 effective lines). Nothing in this crate could parse a `subagent/descriptor`
//! payload or fold one: `grep -rn "SubagentDescriptor\|settled_ms\|SubagentIdentity" crates/` was empty
//! before this file existed, and [`crate::subagent::extra`] names exactly this as its "gap 3" it
//! deliberately did not port.
//!
//! The second gap is `--expand-environment=no` probing in `subprocess-local/src/linux-scope.ts`; it is
//! left out because it is a *driver* behaviour (try one argv, parse `unrecognized option` out of
//! `systemd-run`'s stderr, fall back), and this build has no `systemd-run` runner to probe with. A
//! parser for a message no local caller can produce would be scaffolding, not coverage.
//!
//! # Why the descriptor is the durable half of composition
//!
//! A child's descriptor is appended *once*, inside its initial turn, before its first request. It is
//! log-only: it never enters model history and survives compaction. Everything a cold resume needs to
//! rebuild the child is in it, field by field: the provider that established the child, its mode
//! (terminal `one-shot` or resumable `continuable`), the creation label a parent lists it under, and —
//! for the continuable arm only — the resolved route, persona, and tool scoping.
//!
//! Two decisions in that schema are load-bearing, and both are enforced here:
//!
//! - **Explicit fields, not a merge of `AgentOptions`.** An unrelated extension value cannot make a
//!   continuation fail merely because it is not JSON, and adding a composition input is a deliberate
//!   version bump rather than an implicit extra field. So an unknown field in a *current-version*
//!   payload is a hard error ([`DescriptorError::UnknownField`]), not something to ignore: ignoring it
//!   would silently drop composition the writer meant the resumer to apply.
//! - **`version` gates everything.** A payload whose version this runtime does not know is *not*
//!   broken; it belongs to a newer (or older) build, and the answer is `Ok(None)` — "this runtime
//!   cannot classify this child" — rather than an error. That is what lets a downgraded harness read a
//!   newer log without claiming the child is damaged.
//!
//! # Why the fold takes the *first* descriptor and the projection takes the *last*
//!
//! These look contradictory and are not. [`fold_subagent_descriptor`] answers "what is this child?",
//! and the establishing provider appends exactly one descriptor, so the first is authoritative — a
//! later same-type event must not rewrite declared composition.
//!
//! [`IdentityFold`] answers "what does the client show for this child?", and it folds a log that may
//! begin with a fork seed containing an *ancestor's* descriptor. Missing that distinction is how a
//! forked child inherits its ancestor's identity; so every descriptor resets, last-wins, and any
//! unusable payload resets to the `None` sentinel instead of keeping the previous value. `None` means
//! "no valid descriptor", and the causes (missing, malformed, unknown version) are deliberately
//! undistinguished.
//!
//! The `seq` carried on [`SubagentIdentity`] is what makes the shown label trustworthy: a projection
//! may hold an identity folded from the inherited prefix, so [`describe_child`] only shows a label when
//! the caller proves the seq is in the child's *own* log suffix (`session.isOwnSeq(seq)` upstream).
//! Without that proof the item is reported by id alone, which is ugly but never wrong.
//!
//! # Why timing is folded around the descriptor
//!
//! [`TimingFold`] accumulates completed-turn milliseconds `settledMs` for the *child*, and the
//! descriptor is its origin. A fork seed replays an ancestor's turn boundaries, so the fold keeps a
//! pre-descriptor `turn/start` as `pendingTurnStart` and only promotes it when the child's own
//! descriptor arrives; a `turn/end` before that drops it without settling anything. Every descriptor
//! also zeroes `settledMs`, because the child's own descriptor is the authoritative start of its clock.
//!
//! `lastTurnCompleted` is a tri-state — absent while a turn is open or before one closes, then
//! `Some(bool)`. It is cleared (not set to `false`) by a new `turn/start`: "still running" and "failed"
//! are different facts, and a client that rendered the latter for the former would report every live
//! child as broken.
//!
//! # What is deliberately not here
//!
//! The `workspace/session-activity` waterfall itself. Upstream's listener answers that channel by
//! merging its family into whatever the next listener returned; this crate has no session-activity
//! channel (see [`crate::workspace::rpc_extra`], which refuses `stopActivity` for exactly that reason),
//! so this module ports the *pure* part — ancestor walking, running-descendant selection, and the
//! own-seq-gated item description — and a caller that owns such a channel can merge
//! [`activity_family`]'s answer into it.

#![allow(dead_code)]

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;

use serde_json::{Map, Value};

/// The descriptor format version this runtime writes and reads.
///
/// Upstream `descriptor.ts`: `SUBAGENT_DESCRIPTOR_VERSION`. A payload at any other version is not an
/// error: it is a log this build cannot classify, which [`parse_subagent_descriptor`] reports as
/// `Ok(None)`.
pub const SUBAGENT_DESCRIPTOR_VERSION: u64 = 3;

/// `SUBAGENT_DESCRIPTOR_VERSION` in the form a JSON number comparison needs.
///
/// JSON has one number type, so `3` and `3.0` are the same value; upstream's `version !== 3` therefore
/// accepts both spellings. Comparing through this constant keeps that behaviour without a lossy
/// integer parse.
const CURRENT_VERSION_AS_NUMBER: f64 = 3.0;

/// Descriptor versions that a *historical* (format v0) log may contain.
///
/// Upstream `session-format-v0-to-v1/src/payload-validation.ts`: `SUBAGENT_DESCRIPTOR_VERSIONS`.
/// Version 3 is what this runtime writes; 1 and 2 are readable only as historical records, and their
/// schema differs (v1 has no `mode`, v2 has no `agentReasoningEffort`), which is why the migration
/// table stays a separate concern from [`parse_subagent_descriptor`].
pub const KNOWN_FORMAT_V0_DESCRIPTOR_VERSIONS: [u64; 3] = [1, 2, 3];

/// Fields every supported descriptor payload may carry.
///
/// Upstream `descriptor.ts`: `DESCRIPTOR_BASE_KEYS`.
const DESCRIPTOR_BASE_KEYS: [&str; 4] = ["version", "mode", "provider", "label"];

/// The complete admitted key set of a `one-shot` payload.
///
/// Upstream `descriptor.ts`: `ONE_SHOT_DESCRIPTOR_KEYS`.
const ONE_SHOT_DESCRIPTOR_KEYS: [&str; 4] = DESCRIPTOR_BASE_KEYS;

/// The complete admitted key set of a `continuable` payload.
///
/// Upstream `descriptor.ts`: `CONTINUABLE_DESCRIPTOR_KEYS`. A `one-shot` child that carried
/// `agentProvider` would be a writer bug: a terminal run has no composition to resume, so admitting
/// the field would let a log claim a resumability that does not exist.
const CONTINUABLE_DESCRIPTOR_KEYS: [&str; 9] = [
    "version",
    "mode",
    "provider",
    "label",
    "agentProvider",
    "agentModel",
    "agentReasoningEffort",
    "persona",
    "toolFilter",
];

/// The complete admitted key set of a tool restriction.
///
/// Upstream `descriptor.ts`: `TOOL_FILTER_KEYS`.
const TOOL_FILTER_KEYS: [&str; 2] = ["allow", "deny"];

/// Whether a session-backed subagent is a terminal run or a resumable conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptorMode {
    /// A terminal run that cannot be cold-resumed after it finishes.
    OneShot,
    /// A conversation whose declared composition supports cold resume.
    Continuable,
}

impl DescriptorMode {
    /// The wire spelling, as it appears in the payload and in [`crate::subagent::extra`]'s catalog
    /// rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OneShot => "one-shot",
            Self::Continuable => "continuable",
        }
    }

    /// Parses a wire spelling. `None` for anything else, which the descriptor parser turns into
    /// [`DescriptorError::Mode`].
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "one-shot" => Some(Self::OneShot),
            "continuable" => Some(Self::Continuable),
            _ => None,
        }
    }
}

impl fmt::Display for DescriptorMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A child's tool scoping, reapplied on resume.
///
/// At least one side is present: an empty filter is refused, because [`parse_subagent_descriptor`]'s
/// job is to notice that a writer meant to constrain tools and wrote nothing, not to quietly allow
/// every tool on the resumed child.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolRestriction {
    /// Tools the child may use, when the writer stated an allow list.
    pub allow: Option<Vec<String>>,
    /// Tools the child may not use, when the writer stated a deny list.
    pub deny: Option<Vec<String>>,
}

impl ToolRestriction {
    /// Whether this restriction states anything at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.allow.is_none() && self.deny.is_none()
    }
}

/// The durable identity and resumable composition of a session-backed subagent child.
///
/// Upstream `descriptor.ts`: the `OneShotSubagentDescriptorData` /
/// `ContinuableSubagentDescriptorData` union. The union is kept as an enum rather than flattened into
/// optional fields so that "a continuable child always has a label" is a fact of the type: a reader
/// cannot ask a one-shot child for the persona it does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentDescriptor {
    /// A terminal one-shot child.
    OneShot {
        /// The `ctx.subagents` provider name that established the child.
        provider: String,
        /// The initial delegation's short description, kept as the durable creation label.
        label: Option<String>,
    },
    /// A child whose declared composition supports cold resume.
    Continuable {
        /// The `ctx.subagents` provider name that established the child.
        provider: String,
        /// The durable creation label. Required here: a resumable child must be nameable.
        label: String,
        /// Resolved child `agentOptions.provider`.
        agent_provider: Option<String>,
        /// Resolved child `agentOptions.model`.
        agent_model: Option<String>,
        /// Resolved child `agentOptions.reasoningEffort`.
        agent_reasoning_effort: Option<String>,
        /// Per-child persona that shadows the deployment persona on resume.
        persona: Option<String>,
        /// Child tool scoping reapplied on resume.
        tool_filter: Option<ToolRestriction>,
    },
}

impl SubagentDescriptor {
    /// Whether the child is terminal or resumable.
    #[must_use]
    pub const fn mode(&self) -> DescriptorMode {
        match self {
            Self::OneShot { .. } => DescriptorMode::OneShot,
            Self::Continuable { .. } => DescriptorMode::Continuable,
        }
    }

    /// The provider that established the child.
    #[must_use]
    pub fn provider(&self) -> &str {
        match self {
            Self::OneShot { provider, .. } | Self::Continuable { provider, .. } => provider,
        }
    }

    /// The durable creation label, when the writer recorded one.
    ///
    /// Only a one-shot child may lack it: `label` is required on the continuable arm, which is why this
    /// returns `Option` rather than the arm's field.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::OneShot { label, .. } => label.as_deref(),
            Self::Continuable { label, .. } => Some(label),
        }
    }

    /// Renders the versioned payload this descriptor round-trips through.
    ///
    /// Upstream `descriptor.ts`: `snapshotSubagentDescriptor`. Fields are emitted in the declared order
    /// and absent optionals are omitted rather than set to `null`, because `null` and "not declared"
    /// are different composition facts and only the latter may inherit a default on resume.
    #[must_use]
    pub fn to_payload(&self) -> Value {
        let mut payload = Map::new();
        payload.insert(
            "version".to_owned(),
            Value::from(SUBAGENT_DESCRIPTOR_VERSION),
        );
        payload.insert("mode".to_owned(), Value::from(self.mode().as_str()));
        payload.insert("provider".to_owned(), Value::from(self.provider()));
        let mut optional = |key: &str, value: Option<&String>| {
            if let Some(value) = value {
                payload.insert(key.to_owned(), Value::from(value.as_str()));
            }
        };
        match self {
            Self::OneShot { label, .. } => optional("label", label.as_ref()),
            Self::Continuable {
                label,
                agent_provider,
                agent_model,
                agent_reasoning_effort,
                persona,
                tool_filter,
                ..
            } => {
                optional("label", Some(label));
                optional("agentProvider", agent_provider.as_ref());
                optional("agentModel", agent_model.as_ref());
                optional("agentReasoningEffort", agent_reasoning_effort.as_ref());
                optional("persona", persona.as_ref());
                if let Some(filter) = tool_filter {
                    let mut restriction = Map::new();
                    if let Some(allow) = &filter.allow {
                        restriction.insert("allow".to_owned(), strings_to_value(allow));
                    }
                    if let Some(deny) = &filter.deny {
                        restriction.insert("deny".to_owned(), strings_to_value(deny));
                    }
                    payload.insert("toolFilter".to_owned(), Value::Object(restriction));
                }
            }
        }
        Value::Object(payload)
    }
}

/// One entry of a JSON string array.
fn strings_to_value(values: &[String]) -> Value {
    Value::Array(
        values
            .iter()
            .map(|value| Value::from(value.as_str()))
            .collect(),
    )
}

/// Why a persisted descriptor payload could not be trusted.
///
/// Every message is upstream's, verbatim, because these strings are what a user sees when a cold resume
/// refuses: a reworded message breaks the search that finds the schema that produced it.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DescriptorError {
    /// The payload was not a JSON object.
    #[error("persisted subagent descriptor payload must be an object")]
    PayloadNotAnObject,

    /// The payload carried no numeric `version`.
    #[error("persisted subagent descriptor version must be a number")]
    VersionNotANumber,

    /// A current-version payload carried a field its declared schema does not admit.
    #[error("persisted subagent descriptor {path} has unknown field \"{field}\"")]
    UnknownField {
        /// Which record the field was found in: `payload`, or `toolFilter`.
        path: &'static str,
        /// The offending field name.
        field: String,
    },

    /// The `mode` discriminant was missing or not a known spelling.
    #[error("persisted subagent descriptor mode must be \"one-shot\" or \"continuable\"")]
    Mode,

    /// `provider` was missing or not a string.
    #[error("persisted subagent descriptor provider must be a string")]
    ProviderNotAString,

    /// `label` was missing on a continuable payload, or was not a string.
    #[error("persisted subagent descriptor label must be a string")]
    LabelNotAString,

    /// One of the optional composition fields was present and not a string.
    #[error("persisted subagent descriptor {field} must be a string")]
    FieldNotAString {
        /// The offending field name.
        field: String,
    },

    /// `toolFilter` was present and not an object.
    #[error("persisted subagent descriptor toolFilter must be an object")]
    ToolFilterNotAnObject,

    /// One side of `toolFilter` was present and not an array of strings.
    #[error("persisted subagent descriptor toolFilter.{key} must be an array of strings")]
    ToolFilterNotAnArray {
        /// The offending side: `allow` or `deny`.
        key: String,
    },

    /// `toolFilter` was an object that constrained nothing.
    #[error("persisted subagent descriptor toolFilter must declare allow and/or deny")]
    ToolFilterEmpty,

    /// The payload was rejected by the serializer before it could be written.
    #[error("subagent descriptor is not losslessly JSON-serializable")]
    NotSerializable,
}

/// Refuses a field outside one versioned record's declared schema.
///
/// Upstream `descriptor.ts`: `assertKnownKeys`. Only the alphabetically first offender is named, so a
/// payload with three unknown fields yields one deterministic message rather than a set that varies
/// with map iteration order.
fn assert_known_keys(
    record: &Map<String, Value>,
    allowed: &[&str],
    path: &'static str,
) -> Result<(), DescriptorError> {
    match record.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(unknown) => Err(DescriptorError::UnknownField {
            path,
            field: unknown.clone(),
        }),
        None => Ok(()),
    }
}

/// Reads one optional string field from a persisted descriptor record.
///
/// Upstream `descriptor.ts`: `optionalString`. Present-but-not-a-string is an error rather than a
/// silent `None`, because "the writer meant nothing here" and "the writer meant something this build
/// cannot read" lead to different resumes.
fn optional_string(
    record: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, DescriptorError> {
    match record.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(DescriptorError::FieldNotAString {
            field: key.to_owned(),
        }),
    }
}

/// Reads one optional string-array field from a persisted tool restriction.
///
/// Upstream `descriptor.ts`: `optionalStringArray`.
fn optional_string_array(
    record: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, DescriptorError> {
    let Some(value) = record.get(key) else {
        return Ok(None);
    };
    let Value::Array(items) = value else {
        return Err(DescriptorError::ToolFilterNotAnArray {
            key: key.to_owned(),
        });
    };
    let mut collected = Vec::with_capacity(items.len());
    for item in items {
        let Value::String(text) = item else {
            return Err(DescriptorError::ToolFilterNotAnArray {
                key: key.to_owned(),
            });
        };
        collected.push(text.clone());
    }
    Ok(Some(collected))
}

/// Validates and reconstructs a persisted tool restriction.
///
/// Upstream `descriptor.ts`: `parseToolFilter`. An object that names neither side is refused: it reads
/// as "constrain the tools" while constraining nothing, and a resume that treated it as "allow all"
/// would grant more than the writer intended.
fn parse_tool_filter(value: &Value) -> Result<ToolRestriction, DescriptorError> {
    let Value::Object(record) = value else {
        return Err(DescriptorError::ToolFilterNotAnObject);
    };
    assert_known_keys(record, &TOOL_FILTER_KEYS, "toolFilter")?;
    let allow = optional_string_array(record, "allow")?;
    let deny = optional_string_array(record, "deny")?;
    let filter = ToolRestriction { allow, deny };
    if filter.is_empty() {
        return Err(DescriptorError::ToolFilterEmpty);
    }
    Ok(filter)
}

/// Validates one persisted descriptor payload for the current runtime.
///
/// Upstream `descriptor.ts`: `parseSubagentDescriptor`. `Ok(None)` means the payload declares a version
/// this build does not know — the child cannot be classified, which is not the same as damaged. An
/// `Err` means a *current-version* payload does not match its own declared schema, which is damage.
///
/// # Errors
///
/// [`DescriptorError`] as described above.
pub fn parse_subagent_descriptor(
    payload: &Value,
) -> Result<Option<SubagentDescriptor>, DescriptorError> {
    let Value::Object(record) = payload else {
        return Err(DescriptorError::PayloadNotAnObject);
    };
    let Some(version) = record.get("version") else {
        return Err(DescriptorError::VersionNotANumber);
    };
    if !version.is_number() {
        return Err(DescriptorError::VersionNotANumber);
    }
    // A number this build cannot read is still a number: upstream compares it and answers "not mine",
    // so a huge or fractional version is an unknown version rather than a malformed payload.
    let current = version
        .as_f64()
        .is_some_and(|number| number.total_cmp(&CURRENT_VERSION_AS_NUMBER) == Ordering::Equal);
    if !current {
        return Ok(None);
    }

    let mode = record
        .get("mode")
        .and_then(Value::as_str)
        .and_then(DescriptorMode::from_wire)
        .ok_or(DescriptorError::Mode)?;
    assert_known_keys(
        record,
        match mode {
            DescriptorMode::OneShot => &ONE_SHOT_DESCRIPTOR_KEYS,
            DescriptorMode::Continuable => &CONTINUABLE_DESCRIPTOR_KEYS,
        },
        "payload",
    )?;

    let provider = match record.get("provider") {
        Some(Value::String(text)) => text.clone(),
        _ => return Err(DescriptorError::ProviderNotAString),
    };

    if mode == DescriptorMode::OneShot {
        let label = optional_string(record, "label")?;
        return Ok(Some(SubagentDescriptor::OneShot { provider, label }));
    }

    let label = match record.get("label") {
        Some(Value::String(text)) => text.clone(),
        _ => return Err(DescriptorError::LabelNotAString),
    };
    let agent_provider = optional_string(record, "agentProvider")?;
    let agent_model = optional_string(record, "agentModel")?;
    let agent_reasoning_effort = optional_string(record, "agentReasoningEffort")?;
    let persona = optional_string(record, "persona")?;
    let tool_filter = match record.get("toolFilter") {
        Some(value) => Some(parse_tool_filter(value)?),
        None => None,
    };
    Ok(Some(SubagentDescriptor::Continuable {
        provider,
        label,
        agent_provider,
        agent_model,
        agent_reasoning_effort,
        persona,
        tool_filter,
    }))
}

/// Validates and detaches a continuable descriptor before it is appended.
///
/// Upstream `descriptor.ts`: `snapshotSubagentDescriptor`. The Rust constructor cannot build a
/// descriptor that is not JSON-representable — there are no floats, no opaque handles, and no trait
/// objects in the payload — so the only refusal left is the serializer's. It is kept because a caller
/// handing this function a payload it did not build (a migrated record, a plugin-supplied value) needs
/// the same answer upstream gives.
///
/// # Errors
///
/// [`DescriptorError::NotSerializable`] when the payload does not survive a JSON round trip;
/// the schema errors of [`parse_subagent_descriptor`] when the round-tripped payload does not re-parse
/// to the same descriptor.
pub fn snapshot_subagent_descriptor(
    descriptor: &SubagentDescriptor,
) -> Result<Value, DescriptorError> {
    let payload = descriptor.to_payload();
    let encoded = serde_json::to_string(&payload).map_err(|_| DescriptorError::NotSerializable)?;
    let decoded: Value =
        serde_json::from_str(&encoded).map_err(|_| DescriptorError::NotSerializable)?;
    match parse_subagent_descriptor(&decoded)? {
        Some(round_tripped) if &round_tripped == descriptor => Ok(decoded),
        _ => Err(DescriptorError::NotSerializable),
    }
}

/// One already-decoded log row: its type tag, its sequence, its wall-clock time, and its payload.
///
/// A borrowed view rather than [`crate::SessionEvent`], because that enum has no
/// `subagent/descriptor` variant (see [`crate::subagent::extra`]'s gap 3). Taking the four facts the
/// folds actually read keeps them testable against a log this crate cannot yet deserialize.
#[derive(Clone, Copy, Debug)]
pub struct LoggedEvent<'a> {
    /// The event's `type` tag.
    pub event_type: &'a str,
    /// The event's sequence number within its log.
    pub seq: u64,
    /// The event's wall-clock time, in milliseconds.
    pub time: u64,
    /// The event's payload.
    pub data: &'a Value,
}

impl LoggedEvent<'_> {
    /// Whether the turn this `turn/end` closes completed normally.
    ///
    /// Upstream reads `event.data.reason.kind`; anything other than `completed` — interrupted, failed,
    /// cancelled — counts as not completed, which is what a client renders as a failure.
    #[must_use]
    pub fn turn_completed(&self) -> bool {
        self.data
            .get("reason")
            .and_then(|reason| reason.get("kind"))
            .and_then(Value::as_str)
            == Some("completed")
    }
}

/// Folds a persisted child log to its supported descriptor.
///
/// Upstream `descriptor.ts`: `foldSubagentDescriptor`. The **first** `subagent/descriptor` event is
/// authoritative — the establishing provider appends exactly one, so a later same-type event cannot
/// rewrite declared composition. An unknown-version first event also stops the fold: a newer build's
/// descriptor says "this child is not classifiable here", and reading a *later* record instead would
/// substitute different composition behind the caller's back.
///
/// # Errors
///
/// [`DescriptorError`] when the first descriptor is a current-version payload that violates its schema.
pub fn fold_subagent_descriptor(
    events: &[LoggedEvent<'_>],
) -> Result<Option<SubagentDescriptor>, DescriptorError> {
    let Some(event) = events
        .iter()
        .find(|event| event.event_type == "subagent/descriptor")
    else {
        return Ok(None);
    };
    parse_subagent_descriptor(event.data)
}

/// Durable identity of one descriptor-backed child: lifecycle mode, creation label, and the seq of the
/// descriptor it was folded from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentIdentity {
    /// A terminal one-shot child, whose label may be absent.
    OneShot {
        /// The durable creation label, when the descriptor carried one.
        label: Option<String>,
        /// Seq of the `subagent/descriptor` event this identity was folded from.
        seq: u64,
    },
    /// A resumable conversation, whose label is required.
    Continuable {
        /// The durable creation label.
        label: String,
        /// Seq of the folded descriptor event.
        seq: u64,
    },
}

impl SubagentIdentity {
    /// The lifecycle mode.
    #[must_use]
    pub const fn mode(&self) -> DescriptorMode {
        match self {
            Self::OneShot { .. } => DescriptorMode::OneShot,
            Self::Continuable { .. } => DescriptorMode::Continuable,
        }
    }

    /// The creation label, when the descriptor carried one.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::OneShot { label, .. } => label.as_deref(),
            Self::Continuable { label, .. } => Some(label),
        }
    }

    /// The seq of the descriptor event this identity came from.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        match self {
            Self::OneShot { seq, .. } | Self::Continuable { seq, .. } => *seq,
        }
    }
}

/// Folds the child's mode and label from `subagent/descriptor` events, last-wins.
///
/// Upstream `projection.ts`: `subagentIdentityProjectionDefinition`. The fold state is deliberately a
/// single `Option` and not a set of facts: every descriptor either replaces the identity or clears it,
/// so a fork that replays an ancestor's descriptor cannot leave the child holding an identity its own
/// descriptor failed to establish.
///
/// A projection fold must never throw, so a malformed or unknown-version payload folds to `None`
/// (upstream's `null` sentinel, chosen so the reset survives a JSON push frame) rather than surfacing
/// [`DescriptorError`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentityFold {
    identity: Option<SubagentIdentity>,
}

impl IdentityFold {
    /// A fold with no identity, as before any descriptor.
    #[must_use]
    pub const fn new() -> Self {
        Self { identity: None }
    }

    /// Applies one event. Only `subagent/descriptor` changes the state.
    pub fn apply(&mut self, event: &LoggedEvent<'_>) {
        if event.event_type != "subagent/descriptor" {
            return;
        }
        self.identity = parse_subagent_descriptor(event.data)
            .ok()
            .flatten()
            .map(|descriptor| identity_of(&descriptor, event.seq));
    }

    /// The current identity, or `None` for "no valid descriptor".
    #[must_use]
    pub fn value(&self) -> Option<&SubagentIdentity> {
        self.identity.as_ref()
    }
}

/// Attaches the folding event's seq to a parsed descriptor.
fn identity_of(descriptor: &SubagentDescriptor, seq: u64) -> SubagentIdentity {
    match descriptor {
        SubagentDescriptor::OneShot { label, .. } => SubagentIdentity::OneShot {
            label: label.clone(),
            seq,
        },
        SubagentDescriptor::Continuable { label, .. } => SubagentIdentity::Continuable {
            label: label.clone(),
            seq,
        },
    }
}

/// A bounded interval of an open turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveInterval {
    /// Start of the open turn, in milliseconds.
    pub since: u64,
    /// Latest event time folded into this projection cut.
    pub through: u64,
}

impl ActiveInterval {
    /// The width of the interval as far as this cut has seen.
    #[must_use]
    pub const fn observed_ms(self) -> u64 {
        self.through.saturating_sub(self.since)
    }
}

/// Durable active-turn timing for one descriptor-backed child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubagentTiming {
    /// Milliseconds accumulated across completed turns after the child's own descriptor.
    pub settled_ms: u64,
    /// Same-cut bounds of the currently open turn, when one has not reached `turn/end`.
    pub active: Option<ActiveInterval>,
    /// Whether the latest closed turn completed normally; absent while a turn is open or before one
    /// closes.
    pub last_turn_completed: Option<bool>,
}

/// Fold state for a subagent's latest timing snapshot.
///
/// Upstream `projection.ts`: `TimingState`. `pending_turn_start` is the pre-descriptor `turn/start` that
/// a fork seed replays; it is promoted when the child's own descriptor arrives and dropped otherwise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimingFold {
    settled_ms: u64,
    active: Option<ActiveInterval>,
    pending_turn_start: Option<u64>,
    descriptor_seen: bool,
    last_turn_completed: Option<bool>,
}

impl TimingFold {
    /// A fold at the start of a log: nothing settled, no open turn, no descriptor seen.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            settled_ms: 0,
            active: None,
            pending_turn_start: None,
            descriptor_seen: false,
            last_turn_completed: None,
        }
    }

    /// Applies one event.
    ///
    /// A fork seed may contain an ancestor's descriptor and completed turns, so every descriptor resets
    /// the accumulated state: the healthy catalog admits only a child with exactly one descriptor in its
    /// own suffix, which makes the final reset the child's authoritative timing origin.
    pub fn apply(&mut self, event: &LoggedEvent<'_>) {
        match event.event_type {
            "turn/start" => {
                // Clearing rather than setting `false`: "open" and "failed" are different answers.
                self.last_turn_completed = None;
                if self.descriptor_seen {
                    self.active = Some(ActiveInterval {
                        since: event.time,
                        through: event.time,
                    });
                } else {
                    self.pending_turn_start = Some(event.time);
                }
            }
            "subagent/descriptor" => {
                let since = self
                    .active
                    .map(|active| active.since)
                    .or(self.pending_turn_start);
                self.descriptor_seen = true;
                self.settled_ms = 0;
                self.pending_turn_start = None;
                self.last_turn_completed = None;
                self.active = since.map(|since| ActiveInterval {
                    since,
                    through: event.time,
                });
            }
            "turn/end" => {
                if !self.descriptor_seen {
                    // A replayed ancestor turn: it belongs to a different clock, so it settles nothing.
                    self.pending_turn_start = None;
                    return;
                }
                let Some(active) = self.active.take() else {
                    return;
                };
                self.settled_ms = self
                    .settled_ms
                    .saturating_add(event.time.saturating_sub(active.since));
                self.last_turn_completed = Some(event.turn_completed());
            }
            _ => {
                // Any other event inside an open turn extends the observed bound.
                if let Some(active) = self.active.as_mut() {
                    active.through = event.time;
                }
            }
        }
    }

    /// The serializable view a projection push frame carries.
    ///
    /// Absent optionals stay absent rather than becoming `null`: the wire schema is strict, and a
    /// `null` `active` would fail it where the omitted field passes.
    #[must_use]
    pub const fn view(&self) -> SubagentTiming {
        SubagentTiming {
            settled_ms: self.settled_ms,
            active: self.active,
            last_turn_completed: self.last_turn_completed,
        }
    }
}

/// One active item of the `subagent` activity family.
///
/// Upstream `types.ts`: `SessionActivityItem`. The label is display-only; identity is `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityItem {
    /// The child's Session id.
    pub id: String,
    /// The child's projected label, when one may be trusted.
    pub label: Option<String>,
}

/// The `subagent` family's answer to `workspace/session-activity`.
///
/// Upstream `types.ts`: `SessionActivity` narrowed to `kind: 'subagent'`, whose items are always
/// present because a subagent family always has per-item identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentActivity {
    /// The running child sessions, in the order their agents were listed.
    pub items: Vec<ActivityItem>,
}

/// The three header facts an ancestry walk follows.
///
/// Upstream `archive-admission.ts` reads these off `agent.session.header`. A borrowed view, for the same
/// reason [`crate::session::top`]'s walk takes one: a walk over three fields should not force the
/// stored-header type and this view to agree on everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildHeader<'a> {
    /// The Session's identity.
    pub id: &'a str,
    /// Whether this Session was spawned as a subagent rather than created by a client.
    pub origin_subagent: bool,
    /// The parent Session, when this one has one.
    pub parent_session: Option<&'a str>,
}

/// The creation ancestry of one Session, nearest parent first.
///
/// Upstream `archive-admission.ts`: `headerAncestors`. Three edges matter and each is a decision:
///
/// - An **offline direct creator** is still an ancestor: the chain is header-only, so a parent whose
///   Agent has been released still appears.
/// - An **ordinary fork stops the chain**. A fork's `origin` is not `subagent`, so its ancestor is not a
///   lifecycle owner and must not be walked *through* — that is what keeps an archived ancestor from
///   blocking a copy that never belonged to it.
/// - A **cycle terminates**: `visited` is seeded with the starting Session, so a corrupt lineage
///   returns the ancestors found so far instead of looping.
#[must_use]
pub fn header_ancestors(
    header: ChildHeader<'_>,
    lookup: impl Fn(&str) -> Option<ChildHeader<'static>>,
) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::from([header.id.to_owned()]);
    let mut current = header;
    loop {
        if !current.origin_subagent {
            return ancestors;
        }
        let Some(parent) = current.parent_session else {
            return ancestors;
        };
        if !visited.insert(parent.to_owned()) {
            return ancestors;
        }
        ancestors.push(parent.to_owned());
        match lookup(parent) {
            Some(next) => current = next,
            // The parent exists as an id but is not resident: the chain ends here rather than guessing.
            None => return ancestors,
        }
    }
}

/// One resident Agent as the activity family sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildAgentView {
    /// The Agent's Session id.
    pub id: String,
    /// Whether the Agent is running.
    pub running: bool,
    /// The Agent's creation ancestry, nearest parent first — what [`header_ancestors`] returns.
    pub ancestors: Vec<String>,
}

/// The running descendants of one Session, in listing order.
///
/// Upstream `archive-admission.ts`: `runningDescendants`. Only `running` children count: a settled child
/// is not work that archiving would have to stop, and reporting it would make every archive prompt for a
/// confirmation that nothing needs.
#[must_use]
pub fn running_descendants(session_id: &str, agents: &[ChildAgentView]) -> Vec<String> {
    agents
        .iter()
        .filter(|agent| agent.running)
        .filter(|agent| {
            agent
                .ancestors
                .iter()
                .any(|ancestor| ancestor == session_id)
        })
        .map(|agent| agent.id.clone())
        .collect()
}

/// Describes one running child for the activity family.
///
/// Upstream `archive-admission.ts`: `describe`. A label is shown only when
/// `is_own_seq(identity.seq)` proves the identity was folded from the child's *own* log suffix; an
/// inherited, absent, or unproven identity yields the id alone. That refusal is the point: a fork of a
/// named child would otherwise be listed twice under one name, and the user stopping "the" child would
/// stop the wrong one.
#[must_use]
pub fn describe_child(
    id: &str,
    identity: Option<&SubagentIdentity>,
    is_own_seq: impl Fn(u64) -> bool,
) -> ActivityItem {
    let label = identity
        .filter(|identity| is_own_seq(identity.seq()))
        .and_then(|identity| identity.label())
        .map(str::to_owned);
    ActivityItem {
        id: id.to_owned(),
        label,
    }
}

/// Builds the `subagent` activity family for one Session, or `None` when nothing is running.
///
/// Upstream `archive-admission.ts`: the `workspace/session-activity` listener, minus the waterfall.
/// `None` is the "not active" answer, and the caller merges its own families as upstream's `next()`
/// would: a family with no items must not appear as an empty row, because an empty row reads as "this
/// session has subagent work" to anything that counts rows.
#[must_use]
pub fn activity_family(
    session_id: &str,
    agents: &[ChildAgentView],
    identity_of: impl Fn(&str) -> Option<SubagentIdentity>,
    is_own_seq: impl Fn(&str, u64) -> bool,
) -> Option<SubagentActivity> {
    let running = running_descendants(session_id, agents);
    if running.is_empty() {
        return None;
    }
    let items = running
        .iter()
        .map(|id| {
            let identity = identity_of(id);
            describe_child(id, identity.as_ref(), |seq| is_own_seq(id, seq))
        })
        .collect();
    Some(SubagentActivity { items })
}
