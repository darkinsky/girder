//! The `v2 → v3` **whole-artifact** session-format migration: a pure array transformation.
//!
//! [`crate::session_migration`] migrates one JSONL row at a time, because that is what a reader has
//! in hand when a line fails to decode. That design excludes two edges outright and reports
//! [`MigrationError::Unsupported`] for them:
//!
//! | edge | upstream size | what makes it whole-artifact |
//! |---|---|---|
//! | `v1 → v2` | 695 lines of `migration.ts`; 1,256 across its `src/` tree | top-level `assistant/chunk` rows fold *into* the matching `assistant/message`, so the row that disappears and the row that grows are different rows; every survivor and every declared sequence reference is then renumbered densely. A row cannot know its position. |
//! | `v2 → v3` | 173 lines of `migration.ts`; 815 across its `src/` tree | a system prompt that rode inside `request/header` becomes a protected **surface head** inserted after the first `step/start`, later prompt changes become replacement rows, and every later `seq` shifts. A row upgrade cannot insert the rows that must precede it. |
//!
//! # The `v2 → v3` rules, one at a time
//!
//! 1. **Promote the prompt.** A `request/header` row's `data.header.system` is removed from the
//!    header and compared with the prompt currently on the head. When it differs, a
//!    `system/message` row is emitted **before** the request row: `{ turn, step }` from the open
//!    step, a plugin-sourced message whose `content` is `[{ "type": "text", "text": prompt }]` (an
//!    empty array for a cleared prompt), `surfaceOp: "append"` for the first head and
//!    `surfaceOp: { op: "replace", startSeq: head, endSeq: head }` plus `sourceEventSeqs: [head]`
//!    for every later change. The head then moves to the new row.
//! 2. **Open the head, empty.** The first `step/start` emits an *empty* head immediately after
//!    itself, with `surfaceOp: "append"`, so the surface is never without a protected head and a
//!    log that stops mid-step still replays. This is deliberate: an empty head contributes nothing
//!    to the replayed request, so inserting it does not change what the model saw.
//! 3. **Canonicalize envelopes.** `surfaceOp: { op: "replace", start, end }` becomes
//!    `{ op: "replace", startSeq, endSeq }`, and an empty `header.tools` array or an empty
//!    `header.adapterDefaults` object is dropped so the row is byte-canonical.
//! 4. **Rename PTC vocabulary.** `agentPreset: "code"` becomes `"ptc"` (in the header and in
//!    `agent-preset/selected`), `tool/code-dispatch-start` becomes `tool/ptc-dispatch-start`,
//!    `tool/code-dispatch` becomes `tool/ptc-dispatch`, and a message `source` of
//!    `{ kind: "plugin", plugin: "tools-code-mode" }` becomes `plugin: "tools-ptc"` wherever a
//!    message is carried (`user/message`, `agent/inbox/spliced.inserted`,
//!    `session/title-llm-request.messages`).
//! 5. **Remap local references, densely.** Every row is renumbered to its output position, and the
//!    *audited same-artifact* references move with it: `command/done.sourceEventSeq`,
//!    `compaction/{summary,prune}.shadowedRange.start|end` and `.shadowedSeqs`,
//!    `session/title.messageSeqs`, `session/title-llm-request.messageSeqs`, the top-level
//!    `sourceEventSeqs`, and `surfaceOp.start|end`. Everything else is opaque and stays
//!    byte-identical: `tool-workflow/*.seq` is a workflow-local counter, a `session-reference`
//!    capture names another generation, stream block indices are not session positions, and numeric
//!    tool argument JSON is model input.
//! 6. **Track the inherited cut.** For a seeded session the `session/end-seed { inherited: true }`
//!    row fixes the cut, but the *target* cut is the count of emitted rows at that point — every
//!    head inserted before the marker shifts it. Upstream derives it rather than copying it.
//!
//! # Why upstream refuses, and why a row-by-row migrator must too
//!
//! Four refusals in `packages/session/session-format-v2-to-v3/src/migration.ts` are not incidental
//! strictness; each names state the edge needs and one row cannot supply:
//!
//! - a `surface` row before the first `step/start` → `surface before first step cannot acquire a
//!   system head without changing chronology`. The head must be emitted *from* a step, so a row
//!   alone has no `{ turn, step }` to stamp on it;
//! - a changed prompt outside an open `step` → `changed request prompt outside an open step cannot
//!   retain source chronology`. There is nowhere in the surface history to put the replacement;
//! - a generated `system/message` id that collides with a source message id → the id is derived from
//!   `sha256(["session-format-v2-to-v3", session id, anchor seq, anchor type])`, so detecting the
//!   collision needs the whole message inventory, not one row;
//! - a `session/end-seed` marker disagreeing with the caller's source cut, or a
//!   `session-log-deepseek/delivery-accepted` marker of the current generation naming another
//!   session → both are statements about the artifact, checked in `finish`.
//!
//! [`crate::session_migration`]'s own doc therefore reports this edge as
//! [`MigrationError::Unsupported`] with the reason "a row upgrade cannot insert the system head or
//! the prompt replacements, and an output missing its protected head is not a valid v3 log" — which
//! is exactly right for a row migrator and exactly the constraint this module removes.
//!
//! # What is and is not portable
//!
//! **Ported here** — all of the above: it is a function from `Vec<Event>` to `Vec<Event>` with no
//! I/O, no clock, and no randomness. The staged shape is kept (feed rows, then `finish`) because the
//! cut and the generated-id collisions are only decidable once every row has been seen.
//!
//! **Not ported** — the ~1,100-line `assertReleasedPayloadSemantics` model-input validator that
//! upstream runs on admitted v2 rows before transforming them. That is vocabulary transcription, not
//! migration logic; this module checks the *envelope*, the audited member inventory, message
//! sources, content block kinds, the surface metadata grammar, and the coordinates it moves. It does
//! not re-derive every field rule for `team/task`, `goal/change`, `hook/result`, or the other
//! opaque payloads, and it does not validate that the result is a legal v3 *artifact* (surface
//! reconstruction, protected-head shadowing). Those gaps are named rather than silently accepted:
//! a caller that needs them should compose [`crate::session_migration`]'s row checks over the
//! opaque payloads, or port the remaining validators.
//!
//! **`v1 → v2` is not implemented**, and not because it is harder to describe: its renumbering is
//! the same one above, but its cardinality change is not. Folding `assistant/chunk` rows into the
//! `assistant/message` they belong to means running the LLM stream accumulator to reconstruct the
//! message, inventing `assistant/attempt` rows for attempts that never produced one, and *deleting*
//! rows — after which every declared reference to a deleted row must be proven absent. Doing that
//! half-way would produce a dense log that silently lost an attempt, which is worse than the
//! refusal. `v2 → v3` inserts rows and renumbers; it never removes one, so it can be ported exactly.

use serde_json::{Map, Value};

/// The generation this module reads.
const SOURCE_VERSION: u64 = 2;

/// The generation this module writes.
const TARGET_VERSION: u64 = 3;

/// The plugin that owns the promoted system message.
const SYSTEM_PLUGIN: &str = "@deepseek-ai/dsh-system-prompt";

/// The migration name that salts every generated message id.
const MIGRATION_NAME: &str = "session-format-v2-to-v3";

/// Prefix of a generated system message id.
const GENERATED_ID_PREFIX: &str = "v2-to-v3-system-";

/// The retired preset name.
const CODE_PRESET: &str = "code";

/// The preset name that replaced it.
const PTC_PRESET: &str = "ptc";

/// The retired code-mode plugin attribution.
const CODE_MODE_PLUGIN: &str = "tools-code-mode";

/// The plugin attribution that replaced it.
const PTC_PLUGIN: &str = "tools-ptc";

/// Event types that carry surface state, and therefore a mandatory `surfaceOp`.
const SURFACE_TYPES: [&str; 4] = [
    "system/message",
    "user/message",
    "assistant/message",
    "tool/result",
];

/// Message `source.kind` values released v2 admits.
const SOURCE_KINDS: [&str; 15] = [
    "user",
    "plugin",
    "model",
    "tool",
    "agent-instructions",
    "session-reference",
    "team-message",
    "goal",
    "skill-invocation",
    "skill-catalog",
    "coordinator",
    "subagent-report",
    "subagent-settled",
    "webhook",
    "agent-message",
];

/// Content block types released v2 admits.
const CONTENT_KINDS: [&str; 6] = [
    "text",
    "reasoning",
    "image",
    "file",
    "tool-call",
    "tool-result",
];

/// The header members released v2 requires and additionally admits.
const HEADER_REQUIRED: [&str; 5] = ["version", "id", "createdAt", "isSeeded", "delegationDepth"];
const HEADER_OPTIONAL: [&str; 4] = ["cwd", "parentSession", "origin", "agentPreset"];

/// The exact payload-member inventory frozen for released v2.
///
/// Transcribed from `RELEASED_V2_EVENT_DISPOSITIONS`, which is released v0's table minus
/// `assistant/chunk` and plus the three members v2 introduced. Upstream builds a record for O(1)
/// lookup; this is a linear scan over 51 entries, which is short enough that a lookup table would
/// cost more to read than it saves to run. The order here is upstream's declaration order, not the
/// alphabetical `RELEASED_V2_EVENT_TYPES` inventory, because nothing in the migration depends on
/// that order.
#[rustfmt::skip]
const V2_EVENT_DISPOSITIONS: [(&str, &[&str], &[&str]); 51] = [
    ("agent-preset/selected", &["agentPreset"], &[]),
    ("agent/inbox/spliced", &["target", "start", "inserted"], &["removedCount", "outcome"]),
    ("approval/asked", &["id", "toolName"], &["callId", "reason"]),
    ("approval/decided", &["id", "outcome"], &[]),
    ("approval/policy", &["policy"], &["source"]),
    ("assistant/attempt", &["turn", "step", "stream"], &[]),
    ("assistant/message", &["turn", "step", "message", "stream"], &["usage", "interrupted"]),
    ("command/done", &["commandId", "kind"], &["text", "sourceEventSeq"]),
    ("command/run", &["commandId", "name", "source"], &["args"]),
    ("compaction/end", &["compactionId", "turn"], &["sourceCommandId", "error"]),
    ("compaction/prune", &["shadowedRange", "shadowedSeqs", "shadowedTokenCount"], &[]),
    ("compaction/start", &["compactionId", "turn"], &["sourceCommandId"]),
    ("compaction/summary", &["compactionId", "summary", "shadowedRange", "shadowedSeqs", "shadowedTokenCount", "provider", "model"], &["sourceCommandId", "maxTokens", "usage", "rawOutput", "llmStreamCall"]),
    ("feedback/record", &["text"], &[]),
    ("goal/change", &["kind", "version", "operation"], &["goal", "roundsStarted", "createdAt", "updatedAt", "cleared", "clearedAt"]),
    ("hook/invoked", &["turn", "point", "dialect", "handlerId"], &["matcher"]),
    ("hook/result", &["turn", "point", "handlerId", "decision", "durationMs"], &["exitCode", "stderrSummary"]),
    ("llm/retry", &["retryId", "turn", "step", "provider", "mode", "policyKey", "retry", "delayMs", "failure"], &["maxRetries"]),
    ("llm/retry-started", &["retryId", "turn", "step", "retry"], &[]),
    ("model/selection", &["provider", "model"], &["reasoningEffort"]),
    ("permission/preset", &["preset"], &[]),
    ("plan/mode", &["active"], &[]),
    ("request/context", &["provider", "model"], &["contextWindow"]),
    ("request/header", &["header", "reason"], &["startsSeries"]),
    ("sandbox/mode", &["mode"], &["source"]),
    ("schedule/change", &["version", "operation"], &["schedule", "id", "acceptedAt"]),
    ("session-log-deepseek/delivery-accepted", &["sessionId", "throughSeq"], &["sessionFormatVersion"]),
    ("session/end-seed", &[], &["inherited"]),
    ("session/title", &["title", "messageSeqs", "source"], &[]),
    ("session/title-llm-request", &["titleProvider", "messageSeqs", "route", "system", "messages", "maxTokens"], &[]),
    ("step/end", &["turn", "step"], &[]),
    ("step/start", &["turn", "step"], &[]),
    ("subagent/descriptor", &["version", "provider"], &["mode", "label", "agentProvider", "agentModel", "agentReasoningEffort", "persona", "toolFilter"]),
    ("subagent/model-selection-policy", &["allowedModels"], &[]),
    ("team/member", &["version", "teamId", "member"], &[]),
    ("team/message/delivered", &["version", "teamId", "messageId", "targetId"], &[]),
    ("team/message/queued", &["version", "teamId", "message"], &[]),
    ("team/task", &["version", "teamId", "task"], &[]),
    ("todo/write", &["todos"], &[]),
    ("tool-workflow/agent-end", &["runId", "seq", "outcome"], &[]),
    ("tool-workflow/agent-start", &["runId", "seq", "label", "childId"], &["phase"]),
    ("tool-workflow/run-end", &["runId", "stopReason"], &[]),
    ("tool-workflow/run-start", &["runId", "name"], &[]),
    ("tool/call", &["turn", "step", "callId", "name", "arguments"], &[]),
    ("tool/code-dispatch", &["rootCallId", "parentCallId", "subCallId", "name", "arguments", "isError", "content"], &[]),
    ("tool/code-dispatch-start", &["rootCallId", "parentCallId", "subCallId", "name", "arguments"], &[]),
    ("tool/result", &["turn", "step", "message"], &["error", "meta"]),
    ("turn/end", &["turn", "reason"], &[]),
    ("turn/start", &["turn"], &[]),
    ("user/message", &["role", "id", "content", "source"], &[]),
    ("web/deepseek-search-llm-request", &["endpoint", "apiVersion", "body"], &[]),
];

/// A refused migration.
///
/// Two variants, because upstream distinguishes two outcomes a caller can act on differently: a
/// malformed row is a corrupt log, while an unclassifiable one is a log this build does not know
/// how to transform and should be handed to a newer harness rather than repaired here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MigrationError {
    /// The row or artifact is well-formed but outside what this edge can safely transform.
    ///
    /// Upstream's `SessionFormatUnsupportedMigrationError`: an unclassified event type, an
    /// unclassified message source or content kind, a surface row before the first step, a changed
    /// prompt outside an open step, or a generated-id collision.
    #[error("cannot safely transform: {reason}")]
    Unsupported {
        /// What this edge will not transform, phrased the way upstream's message reads.
        reason: String,
    },
    /// The row or artifact is not the released shape the edge was written for.
    ///
    /// Upstream's `SessionFormatError`: a non-dense source, a missing or unexpected member, a
    /// reference to a row that is not earlier, a generation claim that is not the source's.
    #[error("invalid format v2 log: {reason}")]
    Format {
        /// The concrete violation.
        reason: String,
    },
}

impl MigrationError {
    /// An [`Self::Unsupported`] refusal.
    fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported {
            reason: reason.into(),
        }
    }

    /// A [`Self::Format`] refusal.
    fn format(reason: impl Into<String>) -> Self {
        Self::Format {
            reason: reason.into(),
        }
    }

    /// Whether the refusal is about a log this build cannot classify, rather than a corrupt one.
    ///
    /// A caller importing another harness's files wants to tell these apart: the first calls for a
    /// newer build, the second for a human.
    #[must_use]
    pub const fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported { .. })
    }
}

/// A released-v2 session header.
///
/// Only the members this edge reads or rewrites are lifted into fields; `origin` and `cwd` are
/// validated for shape and then carried verbatim, because nothing in `v2 → v3` changes them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Header {
    /// Format generation. Must be [`SOURCE_VERSION`] on input.
    pub version: u64,
    /// Session identity. Salts every generated system message id.
    pub id: String,
    /// Creation time in milliseconds.
    pub created_at: i64,
    /// Whether this session was seeded from a parent.
    pub is_seeded: bool,
    /// Delegation depth below the root session.
    pub delegation_depth: usize,
    /// Absolute working directory, when recorded.
    pub cwd: Option<String>,
    /// Parent session id, when this session was seeded.
    pub parent_session: Option<String>,
    /// `origin`; released v2 admits only `"subagent"`.
    pub origin: Option<String>,
    /// Agent preset. `"code"` is renamed to `"ptc"` by this edge.
    pub agent_preset: Option<String>,
}

/// A surface replacement's target coordinates.
///
/// Held in *target* coordinates from the moment it is built: [`Event::from_value`] reads the
/// released-v2 `start`/`end` spelling and [`Event::to_value`] writes the canonical v3
/// `startSeq`/`endSeq` spelling, so the struct never carries a half-migrated value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceOp {
    /// Append to the surface. Serialized as the string `"append"`.
    Append,
    /// Replace the inclusive surface range `start_seq..=end_seq`, both earlier than the row.
    Replace {
        /// First replaced target position.
        start_seq: usize,
        /// Last replaced target position.
        end_seq: usize,
    },
}

/// One logical session event, in envelope form.
///
/// `data` is a [`Map`] rather than a [`Value`] because every admitted v2 event's payload is an
/// object, and the transformations are all member edits.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// The event tag.
    pub type_: String,
    /// Dense source position, later the target position.
    pub seq: usize,
    /// Wall-clock milliseconds.
    pub time: i64,
    /// The event payload.
    pub data: Map<String, Value>,
    /// `ignorable`, when present. Released v2 admits only `true`.
    pub ignorable: Option<bool>,
    /// Declared same-artifact references. Surface rows only.
    pub source_event_seqs: Option<Vec<usize>>,
    /// The surface marker. Mandatory for [`SURFACE_TYPES`].
    pub surface_op: Option<SurfaceOp>,
}

/// A session artifact: header, events, and the inherited cut.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionArtifact {
    /// The migrated header.
    pub header: Header,
    /// How many leading events are inherited from the seed.
    pub inherited_event_count: usize,
    /// The migrated events, densely numbered from zero.
    pub events: Vec<Event>,
}

// ---------------------------------------------------------------------------------------------
// JSON helpers. Released v2 is strict about shapes, so every read is checked rather than coerced.
// ---------------------------------------------------------------------------------------------

/// Require an object, naming the subject the way upstream's diagnostics do.
fn as_object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>, MigrationError> {
    value
        .as_object()
        .ok_or_else(|| MigrationError::format(format!("{label} must be an object")))
}

/// Require a non-negative safe integer, the only shape a session position may take.
fn count(value: Option<&Value>, label: &str) -> Result<usize, MigrationError> {
    let number = value.and_then(Value::as_u64).ok_or_else(|| {
        MigrationError::format(format!("{label} must be a non-negative safe integer"))
    })?;
    usize::try_from(number)
        .ok()
        .filter(|_| number <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            MigrationError::format(format!("{label} must be a non-negative safe integer"))
        })
}

/// Require a safe integer, which upstream admits in either sign (times may precede the epoch).
fn safe_integer(value: Option<&Value>, label: &str) -> Result<i64, MigrationError> {
    value
        .and_then(Value::as_i64)
        .filter(|number| number.unsigned_abs() <= MAX_SAFE_INTEGER)
        .ok_or_else(|| MigrationError::format(format!("{label} must be a safe integer")))
}

/// `Number.MAX_SAFE_INTEGER`, the bound every released coordinate respects.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Reject missing and unaudited members rather than guessing whether they contain coordinates.
fn require_keys(
    value: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
    label: &str,
) -> Result<(), MigrationError> {
    if let Some(missing) = required.iter().find(|key| !value.contains_key(**key)) {
        return Err(MigrationError::format(format!(
            "{label} lacks required field {missing}"
        )));
    }
    if let Some(unexpected) = value
        .keys()
        .find(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err(MigrationError::format(format!(
            "{label} has unexpected field {unexpected}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Event envelopes.
// ---------------------------------------------------------------------------------------------

impl Event {
    /// Read one released-v2 row.
    ///
    /// The envelope check is the one upstream's `assertEvent(event, 2)` performs: exact top-level
    /// members, a dense `seq` (checked by the stage, not here), a safe `time`, and — for a surface
    /// row — the mandatory `surfaceOp` with its released `start`/`end` spelling.
    pub fn from_value(value: &Value) -> Result<Self, MigrationError> {
        let row = as_object(value, "format v2 event")?;
        let type_ = row
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| MigrationError::format("format v2 event type must be a string"))?;
        let surface = SURFACE_TYPES.contains(&type_);
        let mut extra: Vec<&str> = vec!["ignorable"];
        if surface {
            extra.extend_from_slice(&["sourceEventSeqs", "surfaceOp"]);
        }
        require_keys(
            row,
            &["type", "seq", "time", "data"],
            &extra,
            &format!("format v2 {type_}"),
        )?;
        let seq = count(row.get("seq"), "event seq")?;
        let time = safe_integer(row.get("time"), "event time")?;
        let ignorable = match row.get("ignorable") {
            None => None,
            Some(Value::Bool(true)) => Some(true),
            Some(_) => {
                return Err(MigrationError::format(format!(
                    "{type_} ignorable must be true"
                )));
            }
        };
        let data = as_object(
            row.get("data")
                .ok_or_else(|| MigrationError::format(format!("{type_} data must be an object")))?,
            &format!("{type_} data"),
        )?
        .clone();
        let source_event_seqs = match row.get("sourceEventSeqs") {
            None => None,
            Some(value) => Some(surface_sources(value, seq, type_)?),
        };
        let surface_op = match row.get("surfaceOp") {
            None => None,
            Some(Value::String(text)) if text == "append" => Some(SurfaceOp::Append),
            Some(value) => {
                let replace = as_object(value, &format!("{type_} {seq} surfaceOp"))?;
                require_keys(
                    replace,
                    &["op", "start", "end"],
                    &[],
                    &format!("{type_} {seq} surfaceOp"),
                )?;
                if replace.get("op").and_then(Value::as_str) != Some("replace") {
                    return Err(MigrationError::format(format!(
                        "{type_} {seq} surfaceOp must replace"
                    )));
                }
                let start_seq = count(
                    replace.get("start"),
                    &format!("{type_} {seq} surface start"),
                )?;
                let end_seq = count(replace.get("end"), &format!("{type_} {seq} surface end"))?;
                if start_seq >= seq || end_seq >= seq {
                    return Err(MigrationError::format(format!(
                        "{type_} {seq} has an invalid surface replacement"
                    )));
                }
                Some(SurfaceOp::Replace { start_seq, end_seq })
            }
        };
        if surface && surface_op.is_none() {
            return Err(MigrationError::format(format!(
                "{type_} requires surfaceOp"
            )));
        }
        Ok(Self {
            type_: type_.to_owned(),
            seq,
            time,
            data,
            ignorable,
            source_event_seqs,
            surface_op,
        })
    }

    /// Write one canonical v3 row.
    ///
    /// Members are inserted in upstream's declaration order; the payload map preserves its own
    /// order, so a payload this edge does not touch round-trips member-for-member.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut row = Map::new();
        row.insert("type".to_owned(), Value::String(self.type_.clone()));
        row.insert("seq".to_owned(), Value::from(self.seq));
        row.insert("time".to_owned(), Value::from(self.time));
        row.insert("data".to_owned(), Value::Object(self.data.clone()));
        if self.ignorable == Some(true) {
            row.insert("ignorable".to_owned(), Value::Bool(true));
        }
        if let Some(sources) = &self.source_event_seqs {
            row.insert(
                "sourceEventSeqs".to_owned(),
                Value::Array(sources.iter().map(|seq| Value::from(*seq)).collect()),
            );
        }
        match self.surface_op {
            None => {}
            Some(SurfaceOp::Append) => {
                row.insert("surfaceOp".to_owned(), Value::String("append".to_owned()));
            }
            Some(SurfaceOp::Replace { start_seq, end_seq }) => {
                let mut replace = Map::new();
                replace.insert("op".to_owned(), Value::String("replace".to_owned()));
                replace.insert("startSeq".to_owned(), Value::from(start_seq));
                replace.insert("endSeq".to_owned(), Value::from(end_seq));
                row.insert("surfaceOp".to_owned(), Value::Object(replace));
            }
        }
        Value::Object(row)
    }
}

/// Validate a declared `sourceEventSeqs` list: unique, earlier, and non-empty.
///
/// `assistant/message` is refused outright because v1 renumbered it to embed its stream, so a
/// surviving list on one would mean chunk references outlived the fold.
fn surface_sources(value: &Value, seq: usize, type_: &str) -> Result<Vec<usize>, MigrationError> {
    if type_ == "assistant/message" {
        return Err(MigrationError::format(format!(
            "assistant/message {seq} retains obsolete chunk references"
        )));
    }
    let members = value.as_array().ok_or_else(|| {
        MigrationError::format(format!("{type_} {seq} sourceEventSeqs must be an array"))
    })?;
    // Released v1 permits one empty list: an `assistant/attempt` whose chunks all folded away.
    // Every other surface row that declares references must declare at least one.
    if members.is_empty() && type_ != "assistant/attempt" {
        return Err(MigrationError::format(format!(
            "{type_} {seq} sourceEventSeqs must be non-empty"
        )));
    }
    let mut seen = Vec::with_capacity(members.len());
    for member in members {
        let current = count(
            Some(member),
            &format!("{type_} {seq} sourceEventSeqs member"),
        )?;
        if current >= seq || seen.contains(&current) {
            return Err(MigrationError::format(format!(
                "{type_} {seq} sourceEventSeqs must be unique earlier seqs"
            )));
        }
        seen.push(current);
    }
    Ok(seen)
}

// ---------------------------------------------------------------------------------------------
// Released-v2 admission.
// ---------------------------------------------------------------------------------------------

/// The frozen inventory entry for an event type, as `(required, optional)` payload members.
fn disposition(type_: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    V2_EVENT_DISPOSITIONS
        .iter()
        .find(|(name, _, _)| *name == type_)
        .map(|(_, required, optional)| (*required, *optional))
}

/// Whether released v2 admits the event at all.
///
/// The two feedback events are admitted without an inventory entry: they carry plugin-owned
/// identities that this edge never interprets, so upstream validates their shape directly.
#[must_use]
pub fn is_admitted_v2_type(type_: &str) -> bool {
    disposition(type_).is_some() || is_feedback(type_)
}

/// Whether the type is a plugin feedback event, which v0's inventory predates.
fn is_feedback(type_: &str) -> bool {
    matches!(type_, "feedback/message-put" | "feedback/message-delete")
}

/// Reject unclassified events, then check the members this edge will read.
///
/// This is the part of `assertEvent(event, 2)` that decides whether a row may be transformed at
/// all. The payload model-input rules upstream runs afterwards are not reproduced here — see the
/// module documentation's "what is and is not portable".
fn admit(event: &Event) -> Result<(), MigrationError> {
    if is_feedback(&event.type_) {
        require_keys(
            &event.data,
            if event.type_ == "feedback/message-put" {
                &["sessionId", "item"]
            } else {
                &["sessionId", "messageId"]
            },
            &[],
            &event.type_,
        )?;
        return Ok(());
    }
    let Some((required, optional)) = disposition(&event.type_) else {
        return Err(MigrationError::unsupported(format!(
            "format v2 to v3 cannot safely transform unclassified event {}",
            event.type_
        )));
    };
    require_keys(&event.data, required, optional, &event.type_)?;
    assert_owned_content(event)?;
    if matches!(
        event.type_.as_str(),
        "assistant/message" | "assistant/attempt"
    ) {
        for coordinate in ["turn", "step"] {
            if count(event.data.get(coordinate), coordinate)? == 0 {
                return Err(MigrationError::format(format!(
                    "{coordinate} must be positive"
                )));
            }
        }
    }
    assert_earlier_references(event)?;
    match event.type_.as_str() {
        "session/end-seed" => {
            if let Some(inherited) = event.data.get("inherited") {
                if inherited.as_bool() != Some(true) {
                    return Err(MigrationError::format(
                        "session/end-seed inherited must be true",
                    ));
                }
            }
        }
        "user/message" => assert_source(&event.data)?,
        "assistant/message" | "tool/result" => {
            let message = as_object(
                event.data.get("message").ok_or_else(|| {
                    MigrationError::format(format!("{} message must be an object", event.type_))
                })?,
                &format!("{} {} message", event.type_, event.seq),
            )?;
            assert_source(message)?;
        }
        "agent/inbox/spliced" | "session/title-llm-request" => {
            let field = if event.type_ == "agent/inbox/spliced" {
                "inserted"
            } else {
                "messages"
            };
            let messages = event
                .data
                .get(field)
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    MigrationError::format(format!("{} {field} must be an array", event.type_))
                })?;
            for (index, message) in messages.iter().enumerate() {
                assert_source(as_object(
                    message,
                    &format!("{} {field}[{index}]", event.type_),
                )?)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Require every audited reference on `event` to name an earlier source row.
///
/// This runs for *every* admitted event, not only the ones the remap moves. The two properties are
/// separate: a reference that names a later row is not a coordinate this edge can move, and a row
/// that carries one is not a legal v2 row — `command/done` naming a `command/run` that comes
/// afterwards would describe a cause that had not happened yet. Upstream refuses it from the
/// payload validator, before the remap ever sees it, so the refusal happens here too rather than
/// being discovered later as an out-of-range lookup.
fn assert_earlier_references(event: &Event) -> Result<(), MigrationError> {
    let seq = event.seq;
    let earlier = |value: Option<&Value>, field: &str| -> Result<(), MigrationError> {
        if let Some(value) = value {
            let source = count(Some(value), field)?;
            if source >= seq {
                return Err(MigrationError::format(format!(
                    "{} {seq} {field} must identify an earlier event",
                    event.type_
                )));
            }
        }
        Ok(())
    };
    let earlier_list = |value: Option<&Value>, field: &str| -> Result<(), MigrationError> {
        if let Some(members) = value.and_then(Value::as_array) {
            for member in members {
                earlier(Some(member), field)?;
            }
        }
        Ok(())
    };
    let earlier_range = |value: Option<&Value>, field: &str| -> Result<(), MigrationError> {
        if let Some(range) = value.and_then(Value::as_object) {
            earlier(range.get("start"), field)?;
            earlier(range.get("end"), field)?;
        }
        Ok(())
    };
    match event.type_.as_str() {
        "command/done" => earlier(event.data.get("sourceEventSeq"), "sourceEventSeq")?,
        "compaction/summary" | "compaction/prune" => {
            earlier_range(event.data.get("shadowedRange"), "shadowedRange")?;
            earlier_list(event.data.get("shadowedSeqs"), "shadowedSeqs")?;
        }
        "session/title" | "session/title-llm-request" => {
            earlier_list(event.data.get("messageSeqs"), "messageSeqs")?;
        }
        _ => {}
    }
    Ok(())
}

/// Require a message source this edge can classify.
fn assert_source(message: &Map<String, Value>) -> Result<(), MigrationError> {
    let source = as_object(
        message
            .get("source")
            .ok_or_else(|| MigrationError::format("message source must be an object"))?,
        "message source",
    )?;
    let kind = source
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| MigrationError::format("message source kind must be a string"))?;
    if !SOURCE_KINDS.contains(&kind) {
        return Err(MigrationError::unsupported(
            "cannot safely transform unclassified message source",
        ));
    }
    if kind == "agent-message" {
        require_keys(
            source,
            &["kind", "form", "senderSessionId"],
            &[],
            "agent-message source",
        )?;
        let relay = source.get("form").and_then(Value::as_str) == Some("relay");
        let sender = source
            .get("senderSessionId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty());
        if !relay || !sender {
            return Err(MigrationError::format(
                "agent-message source requires relay form and senderSessionId",
            ));
        }
    }
    Ok(())
}

/// Require every owned content array to name an audited block type.
fn assert_owned_content(event: &Event) -> Result<(), MigrationError> {
    let label = format!("format v2 {} at seq {} data", event.type_, event.seq);
    match event.type_.as_str() {
        "user/message" | "tool/code-dispatch" => {
            assert_content_kinds(event.data.get("content"), &format!("{label}.content"))?;
        }
        "assistant/message" | "tool/result" | "team/message/queued" => {
            let message = as_object(
                event.data.get("message").ok_or_else(|| {
                    MigrationError::format(format!("{label}.message must be an object"))
                })?,
                &format!("{label}.message"),
            )?;
            assert_content_kinds(message.get("content"), &format!("{label}.message.content"))?;
        }
        "agent/inbox/spliced" | "session/title-llm-request" => {
            let field = if event.type_ == "agent/inbox/spliced" {
                "inserted"
            } else {
                "messages"
            };
            let messages = event
                .data
                .get(field)
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    MigrationError::format(format!("{label}.{field}: content must be an array"))
                })?;
            for (index, message) in messages.iter().enumerate() {
                let path = format!("{label}.{field}[{index}]");
                let message = as_object(message, &path)?;
                assert_content_kinds(message.get("content"), &format!("{path}.content"))?;
            }
        }
        "compaction/summary" => {
            assert_content_kinds(event.data.get("summary"), &format!("{label}.summary"))?;
            if let Some(raw) = event.data.get("rawOutput") {
                assert_content_kinds(Some(raw), &format!("{label}.rawOutput"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Require a content array of audited blocks.
fn assert_content_kinds(value: Option<&Value>, label: &str) -> Result<(), MigrationError> {
    let blocks = value
        .and_then(Value::as_array)
        .ok_or_else(|| MigrationError::format(format!("{label}: content must be an array")))?;
    for (index, block) in blocks.iter().enumerate() {
        assert_content_block(block, &format!("{label}[{index}]"))?;
    }
    Ok(())
}

/// Require one audited content block, recursing through `tool-result` children and checking the
/// `file` attachment's exact member set.
fn assert_content_block(value: &Value, label: &str) -> Result<(), MigrationError> {
    let block = as_object(value, label)?;
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| MigrationError::format(format!("{label} type must be a string")))?;
    if !CONTENT_KINDS.contains(&kind) {
        return Err(MigrationError::unsupported(format!(
            "{label}: cannot safely transform unclassified message content kind {kind:?}"
        )));
    }
    match kind {
        "tool-result" => assert_content_kinds(block.get("content"), &format!("{label}.content"))?,
        "file" => {
            require_keys(
                block,
                &["type", "attachment"],
                &[],
                &format!("{label} kind \"file\""),
            )?;
            let attachment = as_object(
                block.get("attachment").ok_or_else(|| {
                    MigrationError::format(format!(
                        "{label} kind \"file\" attachment must be an object"
                    ))
                })?,
                &format!("{label} kind \"file\" attachment"),
            )?;
            require_keys(
                attachment,
                &["attachmentId", "name", "bytes"],
                &[],
                &format!("{label} kind \"file\" attachment"),
            )?;
            let attachment_id = attachment.get("attachmentId").and_then(Value::as_str);
            let name = attachment.get("name").and_then(Value::as_str);
            if attachment_id.is_none_or(str::is_empty) || name.is_none() {
                return Err(MigrationError::format(format!(
                    "{label} kind \"file\": file attachment requires attachmentId and name"
                )));
            }
            count(
                attachment.get("bytes"),
                &format!("{label} kind \"file\" attachment bytes"),
            )?;
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Header.
// ---------------------------------------------------------------------------------------------

impl Header {
    /// Read a released-v2 header.
    pub fn from_value(value: &Value) -> Result<Self, MigrationError> {
        let record = as_object(value, "format v2 header")?;
        require_keys(
            record,
            &HEADER_REQUIRED,
            &HEADER_OPTIONAL,
            "format v2 header",
        )?;
        if let Some(found) = record.get("version").and_then(Value::as_u64) {
            if found != SOURCE_VERSION {
                return Err(MigrationError::format(format!(
                    "expected format v2 header, found version {found}"
                )));
            }
        } else {
            return Err(MigrationError::format("expected format v2 header"));
        }
        let id = record
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| MigrationError::format("format v2 header id must be a string"))?;
        let created_at = count(record.get("createdAt"), "format v2 header createdAt")?;
        let delegation_depth = count(
            record.get("delegationDepth"),
            "format v2 header delegationDepth",
        )?;
        let is_seeded = record
            .get("isSeeded")
            .and_then(Value::as_bool)
            .ok_or_else(|| MigrationError::format("format v2 header isSeeded must be boolean"))?;
        let cwd = match record.get("cwd") {
            None => None,
            Some(value) => {
                let path = value.as_str().ok_or_else(|| {
                    MigrationError::format("format v2 header cwd must be absolute")
                })?;
                if !path.starts_with('/') {
                    return Err(MigrationError::format(
                        "format v2 header cwd must be absolute",
                    ));
                }
                Some(path.to_owned())
            }
        };
        let optional_string = |key: &str| -> Result<Option<String>, MigrationError> {
            match record.get(key) {
                None => Ok(None),
                Some(value) => value
                    .as_str()
                    .map(|text| Some(text.to_owned()))
                    .ok_or_else(|| {
                        MigrationError::format(format!("format v2 header {key} must be a string"))
                    }),
            }
        };
        let parent_session = optional_string("parentSession")?;
        let agent_preset = optional_string("agentPreset")?;
        let origin = optional_string("origin")?;
        if origin.as_deref().is_some_and(|value| value != "subagent") {
            return Err(MigrationError::format(
                "format v2 header origin must be \"subagent\"",
            ));
        }
        Ok(Self {
            version: SOURCE_VERSION,
            id: id.to_owned(),
            created_at: i64::try_from(created_at).map_err(|_| {
                MigrationError::format("format v2 header createdAt must be a safe integer")
            })?,
            is_seeded,
            delegation_depth,
            cwd,
            parent_session,
            origin,
            agent_preset,
        })
    }

    /// Promote a released-v2 header to v3, renaming the retired `code` preset.
    ///
    /// The rename is exact: `"code"` becomes `"ptc"` and every other value — including
    /// `"code-custom"` and `"Code"` — is carried unchanged, because upstream matches one literal
    /// rather than a prefix.
    pub fn migrate(&self) -> Result<Self, MigrationError> {
        if self.version != SOURCE_VERSION {
            return Err(MigrationError::format(format!(
                "expected format v2 header, found version {}",
                self.version
            )));
        }
        let mut target = self.clone();
        target.version = TARGET_VERSION;
        target.agent_preset = target.agent_preset.take().map(|preset| {
            if preset == CODE_PRESET {
                PTC_PRESET.to_owned()
            } else {
                preset
            }
        });
        Ok(target)
    }

    /// Write a canonical v3 header, omitting absent optional members.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut record = Map::new();
        record.insert("version".to_owned(), Value::from(self.version));
        record.insert("id".to_owned(), Value::String(self.id.clone()));
        record.insert("createdAt".to_owned(), Value::from(self.created_at));
        record.insert("isSeeded".to_owned(), Value::Bool(self.is_seeded));
        record.insert(
            "delegationDepth".to_owned(),
            Value::from(self.delegation_depth),
        );
        for (key, value) in [
            ("cwd", &self.cwd),
            ("parentSession", &self.parent_session),
            ("origin", &self.origin),
            ("agentPreset", &self.agent_preset),
        ] {
            if let Some(text) = value {
                record.insert(key.to_owned(), Value::String(text.clone()));
            }
        }
        Value::Object(record)
    }
}

// ---------------------------------------------------------------------------------------------
// SHA-256, needed only to mint the deterministic system message id.
// ---------------------------------------------------------------------------------------------

/// SHA-256 round constants. FIPS 180-4 §4.2.2.
#[rustfmt::skip]
const SHA256_K: [u32; 64] = [
    0x428a_2f98, 0x7137_4491, 0xb5c0_fbcf, 0xe9b5_dba5, 0x3956_c25b, 0x59f1_11f1, 0x923f_82a4, 0xab1c_5ed5,
    0xd807_aa98, 0x1283_5b01, 0x2431_85be, 0x550c_7dc3, 0x72be_5d74, 0x80de_b1fe, 0x9bdc_06a7, 0xc19b_f174,
    0xe49b_69c1, 0xefbe_4786, 0x0fc1_9dc6, 0x240c_a1cc, 0x2de9_2c6f, 0x4a74_84aa, 0x5cb0_a9dc, 0x76f9_88da,
    0x983e_5152, 0xa831_c66d, 0xb003_27c8, 0xbf59_7fc7, 0xc6e0_0bf3, 0xd5a7_9147, 0x06ca_6351, 0x1429_2967,
    0x27b7_0a85, 0x2e1b_2138, 0x4d2c_6dfc, 0x5338_0d13, 0x650a_7354, 0x766a_0abb, 0x81c2_c92e, 0x9272_2c85,
    0xa2bf_e8a1, 0xa81a_664b, 0xc24b_8b70, 0xc76c_51a3, 0xd192_e819, 0xd699_0624, 0xf40e_3585, 0x106a_a070,
    0x19a4_c116, 0x1e37_6c08, 0x2748_774c, 0x34b0_bcb5, 0x391c_0cb3, 0x4ed8_aa4a, 0x5b9c_ca4f, 0x682e_6ff3,
    0x748f_82ee, 0x78a5_636f, 0x84c8_7814, 0x8cc7_0208, 0x90be_fffa, 0xa450_6ceb, 0xbef9_a3f7, 0xc671_78f2,
];

/// The SHA-256 initial hash value. FIPS 180-4 §5.3.3.
const SHA256_H: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// The lowercase hex SHA-256 of `data`.
///
/// `sha2` is not a dependency of this crate and a generated message id is the only consumer, so the
/// digest is computed here rather than adding one. It is checked against published vectors in this
/// module's test suite.
#[allow(
    clippy::many_single_char_names,
    reason = "the round variable names are the ones FIPS 180-4 uses; renaming them would hide a transcription error"
)]
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    let bit_length = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let mut state = SHA256_H;
    let mut words = [0_u32; 64];
    for chunk in padded.chunks_exact(64) {
        for (word, group) in words.iter_mut().zip(chunk.chunks_exact(4)) {
            *word = u32::from_be_bytes([group[0], group[1], group[2], group[3]]);
        }
        for round in 16..64 {
            let (head, tail) = words.split_at_mut(round);
            let s0 = head[round - 15].rotate_right(7)
                ^ head[round - 15].rotate_right(18)
                ^ (head[round - 15] >> 3);
            let s1 = head[round - 2].rotate_right(17)
                ^ head[round - 2].rotate_right(19)
                ^ (head[round - 2] >> 10);
            if let Some(slot) = tail.first_mut() {
                *slot = head[round - 16]
                    .wrapping_add(s0)
                    .wrapping_add(head[round - 7])
                    .wrapping_add(s1);
            }
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];
        for (&constant, &word) in SHA256_K.iter().zip(words.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(constant)
                .wrapping_add(word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, round) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(round);
        }
    }

    let mut out = String::with_capacity(64);
    for word in state {
        for byte in word.to_be_bytes() {
            out.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
            out.push(char::from(b"0123456789abcdef"[usize::from(byte & 0x0f)]));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// PTC renaming and envelope canonicalization.
// ---------------------------------------------------------------------------------------------

/// Rename the retired `code`-mode vocabulary.
///
/// Source admission precedes this, so each payload here has exact audited members; the rename
/// touches only the three slots upstream identifies as plugin attribution, never a message id, a
/// tool argument, or any other payload.
fn rename_ptc(event: Event) -> Event {
    match event.type_.as_str() {
        "agent-preset/selected" => {
            if event.data.get("agentPreset").and_then(Value::as_str) == Some(CODE_PRESET) {
                let mut next = event;
                next.data.insert(
                    "agentPreset".to_owned(),
                    Value::String(PTC_PRESET.to_owned()),
                );
                next
            } else {
                event
            }
        }
        "tool/code-dispatch-start" => Event {
            type_: "tool/ptc-dispatch-start".to_owned(),
            ..event
        },
        "tool/code-dispatch" => Event {
            type_: "tool/ptc-dispatch".to_owned(),
            ..event
        },
        "user/message" => {
            let mut next = event;
            next.data = rename_message_source(next.data);
            next
        }
        "agent/inbox/spliced" | "session/title-llm-request" => {
            let field = if event.type_ == "agent/inbox/spliced" {
                "inserted"
            } else {
                "messages"
            };
            let mut next = event;
            if let Some(Value::Array(messages)) = next.data.get(field).cloned() {
                let renamed: Vec<Value> = messages
                    .into_iter()
                    .map(|message| match message {
                        Value::Object(record) => Value::Object(rename_message_source(record)),
                        other => other,
                    })
                    .collect();
                next.data.insert(field.to_owned(), Value::Array(renamed));
            }
            next
        }
        _ => event,
    }
}

/// Rename one message's `tools-code-mode` attribution, leaving every other source alone.
fn rename_message_source(mut message: Map<String, Value>) -> Map<String, Value> {
    let renamed = match message.get("source") {
        Some(Value::Object(source)) => {
            if source.get("kind").and_then(Value::as_str) == Some("plugin")
                && source.get("plugin").and_then(Value::as_str) == Some(CODE_MODE_PLUGIN)
            {
                let mut next = source.clone();
                next.insert("plugin".to_owned(), Value::String(PTC_PLUGIN.to_owned()));
                Some(next)
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(source) = renamed {
        message.insert("source".to_owned(), Value::Object(source));
    }
    message
}

/// Canonicalize a structurally transformed event without changing its target coordinates.
///
/// Two rewrites, both of which only shrink or rename: the replaced surface range gains its
/// canonical member names, and empty optional request-header fields are dropped. Upstream then
/// validates that the result is a legal v3 row; that validation is the artifact-level work named in
/// the module docs as not ported.
fn canonicalize(event: &mut Event) {
    if event.type_ == "request/header" {
        if let Some(Value::Object(header)) = event.data.get("header").cloned() {
            let empty_tools = header
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty);
            let empty_defaults = header
                .get("adapterDefaults")
                .and_then(Value::as_object)
                .is_some_and(Map::is_empty);
            if empty_tools || empty_defaults {
                let mut canonical = header;
                if empty_tools {
                    canonical.remove("tools");
                }
                if empty_defaults {
                    canonical.remove("adapterDefaults");
                }
                event
                    .data
                    .insert("header".to_owned(), Value::Object(canonical));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Local-coordinate remapping.
// ---------------------------------------------------------------------------------------------

/// Remap one audited same-artifact reference through the source-to-target table.
///
/// A reference must name an *earlier* source row that has already been mapped; anything else means
/// the log's declared history is not the history being migrated.
fn remap_one(
    mapping: &[usize],
    source: Option<&Value>,
    seq: usize,
) -> Result<usize, MigrationError> {
    let source = count(source, "source event reference")?;
    let target = mapping.get(source).copied();
    match target {
        Some(target) if source < seq => Ok(target),
        _ => Err(MigrationError::format(
            "reference must name an earlier source event",
        )),
    }
}

/// Remap a list of audited references.
fn remap_list(
    mapping: &[usize],
    source: Option<&Value>,
    seq: usize,
) -> Result<Value, MigrationError> {
    let members = source
        .and_then(Value::as_array)
        .ok_or_else(|| MigrationError::format("sequence references must be an array"))?;
    let mut out = Vec::with_capacity(members.len());
    for member in members {
        out.push(Value::from(remap_one(mapping, Some(member), seq)?));
    }
    Ok(Value::Array(out))
}

/// Remap a `{ start, end }` range, preserving the record's other members.
fn remap_range(
    mapping: &[usize],
    source: Option<&Value>,
    seq: usize,
) -> Result<Value, MigrationError> {
    let range = as_object(
        source.ok_or_else(|| MigrationError::format("sequence range must be an object"))?,
        "sequence range",
    )?;
    let mut out = range.clone();
    out.insert(
        "start".to_owned(),
        Value::from(remap_one(mapping, range.get("start"), seq)?),
    );
    out.insert(
        "end".to_owned(),
        Value::from(remap_one(mapping, range.get("end"), seq)?),
    );
    Ok(Value::Object(out))
}

/// Remap only audited same-artifact references, preserving ids and embedded model input.
///
/// The refusals are the point: a `sourceEventSeq` that names a row this artifact does not contain,
/// or names a row at or after itself, is not a reference this edge can move, and leaving it alone
/// would make the target log claim a position that means something else.
fn remap(mapping: &[usize], source: &Event, seq: usize) -> Result<Event, MigrationError> {
    let mut data = source.data.clone();
    match source.type_.as_str() {
        "command/done" => {
            if let Some(value) = data.get("sourceEventSeq").cloned() {
                data.insert(
                    "sourceEventSeq".to_owned(),
                    Value::from(remap_one(mapping, Some(&value), seq)?),
                );
            }
        }
        "compaction/summary" | "compaction/prune" => {
            data.insert(
                "shadowedRange".to_owned(),
                remap_range(mapping, data.get("shadowedRange"), seq)?,
            );
            data.insert(
                "shadowedSeqs".to_owned(),
                remap_list(mapping, data.get("shadowedSeqs"), seq)?,
            );
        }
        "session/title" | "session/title-llm-request" => {
            data.insert(
                "messageSeqs".to_owned(),
                remap_list(mapping, data.get("messageSeqs"), seq)?,
            );
        }
        // Delivery watermarks and session-reference captures identify their original generation.
        // Workflow seq, stream block indices, turn/step, and numeric tool JSON are not session
        // positions and must survive byte-identical.
        _ => {}
    }
    let source_event_seqs = match &source.source_event_seqs {
        None => None,
        Some(members) => {
            let mut out = Vec::with_capacity(members.len());
            for member in members {
                out.push(remap_one(mapping, Some(&Value::from(*member)), seq)?);
            }
            Some(out)
        }
    };
    let surface_op = match source.surface_op {
        None => None,
        Some(SurfaceOp::Append) => Some(SurfaceOp::Append),
        Some(SurfaceOp::Replace { start_seq, end_seq }) => Some(SurfaceOp::Replace {
            start_seq: remap_one(mapping, Some(&Value::from(start_seq)), seq)?,
            end_seq: remap_one(mapping, Some(&Value::from(end_seq)), seq)?,
        }),
    };
    Ok(Event {
        type_: source.type_.clone(),
        seq,
        time: source.time,
        data,
        ignorable: source.ignorable,
        source_event_seqs,
        surface_op,
    })
}

// ---------------------------------------------------------------------------------------------
// The stage.
// ---------------------------------------------------------------------------------------------

/// The `v2 → v3` migration, as a state machine over incoming rows.
///
/// Feed every row to [`Self::transform_event`] in dense source order, then call [`Self::finish`].
/// The stage is kept rather than collapsed to one function because two of the edge's decisions —
/// the target inherited cut and the generated-id collision check — are only decidable after the
/// last row, and because refusing at the row that caused the problem names the problem better than
/// refusing at the end of a log.
#[derive(Debug, Clone)]
pub struct V2ToV3 {
    target: Header,
    mapping: Vec<usize>,
    original_ids: Vec<String>,
    generated_ids: Vec<String>,
    target_seq: usize,
    source_cut: Option<usize>,
    target_cut: Option<usize>,
    last_foreign_delivery_seq: Option<usize>,
    step: Option<(usize, usize)>,
    head: Option<usize>,
    prompt: String,
    events: Vec<Event>,
    /// The cut the source declared, when the caller supplied one.
    source_inherited_event_count: Option<usize>,
}

impl V2ToV3 {
    /// Start a migration from a released-v2 header.
    ///
    /// `source_inherited_event_count` is the artifact's declared inherited count. Upstream takes
    /// `undefined` for a seeded log whose cut is only discoverable from its `session/end-seed`
    /// marker; pass [`None`] for that case and [`Some`] otherwise.
    pub fn new(
        header: &Header,
        source_inherited_event_count: Option<usize>,
    ) -> Result<Self, MigrationError> {
        let target = header.migrate()?;
        // An unseeded log has a cut of zero by construction; a seeded one either states its cut or
        // derives it from the marker.
        let (source_cut, target_cut) = if header.is_seeded {
            (None, None)
        } else {
            (Some(0), Some(0))
        };
        Ok(Self {
            target,
            mapping: Vec::new(),
            original_ids: Vec::new(),
            generated_ids: Vec::new(),
            target_seq: 0,
            source_cut,
            target_cut,
            last_foreign_delivery_seq: None,
            step: None,
            head: None,
            prompt: String::new(),
            events: Vec::new(),
            source_inherited_event_count,
        })
    }

    /// The migrated header.
    #[must_use]
    pub const fn header(&self) -> &Header {
        &self.target
    }

    /// The rows emitted so far.
    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Migrate one row.
    ///
    /// The row is consumed rather than borrowed: the stage owns its output, and cloning a payload
    /// only to drop the source would double the memory a large log needs for no gain.
    pub fn transform_event(&mut self, event: Event) -> Result<(), MigrationError> {
        if event.seq != self.mapping.len() {
            return Err(MigrationError::format(
                "format v2 source events must be dense",
            ));
        }
        admit(&event)?;
        self.observe_message_ids(&event)?;

        let Event {
            type_,
            seq,
            time,
            mut data,
            ignorable,
            source_event_seqs,
            surface_op,
        } = event;

        // Read the coordinates a `step/start` will hand to a head before the payload moves.
        let opens_step = type_ == "step/start";
        let step = if opens_step {
            Some((
                count(data.get("turn"), "turn")?,
                count(data.get("step"), "step")?,
            ))
        } else {
            None
        };

        if type_ == "request/header" {
            let header = as_object(
                data.get("header")
                    .ok_or_else(|| MigrationError::format("request header must be an object"))?,
                "request header",
            )?;
            let prompt = header
                .get("system")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if prompt != self.prompt {
                self.emit_system(&prompt, seq, &type_, time)?;
            }
            let mut stripped = header.clone();
            stripped.remove("system");
            data.insert("header".to_owned(), Value::Object(stripped));
        }
        if SURFACE_TYPES.contains(&type_.as_str()) && self.head.is_none() {
            return Err(MigrationError::unsupported(
                "format v2 surface before first step cannot acquire a system head without changing chronology",
            ));
        }
        if type_ == "session/end-seed"
            && data.get("inherited").and_then(Value::as_bool) == Some(true)
        {
            if !self.target.is_seeded {
                return Err(MigrationError::format(
                    "format v2 unseeded Session contains an inherited end-seed marker",
                ));
            }
            self.source_cut = Some(seq);
            self.target_cut = Some(self.target_seq);
        }
        if type_ == "session-log-deepseek/delivery-accepted" {
            let version = data.get("sessionFormatVersion").and_then(Value::as_u64);
            if version == Some(TARGET_VERSION) {
                return Err(MigrationError::format(
                    "format v2 delivery marker claims target format v3",
                ));
            }
            let session = data.get("sessionId").and_then(Value::as_str);
            if version == Some(SOURCE_VERSION) && session != Some(self.target.id.as_str()) {
                self.last_foreign_delivery_seq = Some(seq);
            }
        }

        let source = Event {
            type_: type_.clone(),
            seq,
            time,
            data,
            ignorable,
            source_event_seqs,
            surface_op,
        };
        let mut target = remap(&self.mapping, &source, self.target_seq)?;
        self.mapping.push(self.target_seq);
        self.target_seq += 1;
        target = rename_ptc(target);
        canonicalize(&mut target);
        self.events.push(target);

        match type_.as_str() {
            "step/start" => {
                self.step = step;
                if self.head.is_none() {
                    self.emit_system("", seq, &type_, time)?;
                }
            }
            "step/end" | "turn/end" => self.step = None,
            _ => {}
        }
        Ok(())
    }

    /// Migrate every row of an artifact-sized batch.
    pub fn transform_events(
        &mut self,
        events: impl IntoIterator<Item = Event>,
    ) -> Result<(), MigrationError> {
        for event in events {
            self.transform_event(event)?;
        }
        Ok(())
    }

    /// Close the migration and report the target inherited cut.
    ///
    /// The three checks here are exactly the ones that need the whole artifact: the marker's source
    /// cut must agree with the caller's, a current-generation delivery marker must name this session
    /// or its parent, and a seeded log must actually state its cut.
    pub fn finish(&self) -> Result<usize, MigrationError> {
        let cut = self.source_cut.ok_or_else(|| {
            MigrationError::format("format v2 inherited end-seed marker must be a count")
        })?;
        if let Some(declared) = self.source_inherited_event_count {
            if declared != cut {
                return Err(MigrationError::format(
                    "format v2 inherited end-seed marker disagrees with its source cut",
                ));
            }
        }
        if let Some(seq) = self.last_foreign_delivery_seq {
            if self.target.parent_session.is_none() || seq >= cut {
                return Err(MigrationError::format(
                    "current-generation delivery marker names the wrong Session",
                ));
            }
        }
        self.target_cut.ok_or_else(|| {
            MigrationError::format(
                "format v3 inherited event count must be a non-negative safe integer",
            )
        })
    }

    /// Finish and return the whole artifact.
    pub fn into_artifact(self) -> Result<SessionArtifact, MigrationError> {
        let inherited_event_count = self.finish()?;
        let Self { target, events, .. } = self;
        Ok(SessionArtifact {
            header: target,
            inherited_event_count,
            events,
        })
    }

    /// Record every message identity the source carries.
    ///
    /// Generated system ids are derived from the session id and an anchor position, so a source
    /// message that already uses one would make the target's identity set ambiguous. Upstream
    /// detects this in either order — a source message may precede *or follow* the generated one —
    /// which is why the whole inventory is kept and why [`Self::emit_system`] checks both sets.
    fn observe_message_ids(&mut self, event: &Event) -> Result<(), MigrationError> {
        match event.type_.as_str() {
            "user/message" => self.record_original(message_id(&event.data)?)?,
            "assistant/message" | "tool/result" => {
                let message = as_object(
                    event.data.get("message").ok_or_else(|| {
                        MigrationError::format(format!("{} message must be an object", event.type_))
                    })?,
                    "message",
                )?;
                self.record_original(message_id(message)?)?;
            }
            "agent/inbox/spliced" | "session/title-llm-request" => {
                let field = if event.type_ == "agent/inbox/spliced" {
                    "inserted"
                } else {
                    "messages"
                };
                for message in event
                    .data
                    .get(field)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    self.record_original(message_id(as_object(message, "message")?)?)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Add a source message id to the inventory.
    ///
    /// A source id that already belongs to a generated head is refused here. This is the half of the
    /// collision that only the source side can see: the head was minted from an *earlier* row, so by
    /// the time this message arrives it is already taken, and recording it would leave the target
    /// with two different messages claiming one identity.
    fn record_original(&mut self, id: String) -> Result<(), MigrationError> {
        if self.generated_ids.iter().any(|generated| generated == &id) {
            return Err(MigrationError::unsupported(
                "source message id collides with a generated system message id",
            ));
        }
        self.original_ids.push(id);
        Ok(())
    }

    /// Emit a system head for `prompt`, anchored at `anchor`.
    ///
    /// The id is deterministic so that migrating the same log twice produces the same bytes, which
    /// is what makes a half-migrated import resumable: a re-run recomputes the same id instead of
    /// minting a second head.
    fn emit_system(
        &mut self,
        prompt: &str,
        anchor_seq: usize,
        anchor_type: &str,
        anchor_time: i64,
    ) -> Result<(), MigrationError> {
        let Some((turn, step)) = self.step else {
            return Err(MigrationError::unsupported(
                "format v2 changed request prompt outside an open step cannot retain source chronology",
            ));
        };
        let identity = serde_json::to_string(&Value::Array(vec![
            Value::String(MIGRATION_NAME.to_owned()),
            Value::String(self.target.id.clone()),
            Value::from(anchor_seq),
            Value::String(anchor_type.to_owned()),
        ]))
        .map_err(|error| {
            MigrationError::format(format!("cannot encode system identity: {error}"))
        })?;
        let id = format!("{GENERATED_ID_PREFIX}{}", sha256_hex(identity.as_bytes()));
        if self.original_ids.iter().any(|original| original == &id)
            || self.generated_ids.iter().any(|generated| generated == &id)
        {
            return Err(MigrationError::unsupported(
                "generated system message id collides with an existing message id",
            ));
        }
        self.generated_ids.push(id.clone());

        let content = if prompt.is_empty() {
            Vec::new()
        } else {
            vec![serde_json::json!({ "type": "text", "text": prompt })]
        };
        let mut data = Map::new();
        data.insert("turn".to_owned(), Value::from(turn));
        data.insert("step".to_owned(), Value::from(step));
        data.insert(
            "message".to_owned(),
            serde_json::json!({
                "id": id,
                "role": "system",
                "source": { "kind": "plugin", "plugin": SYSTEM_PLUGIN },
                "content": content,
            }),
        );
        let seq = self.target_seq;
        self.target_seq += 1;
        let surface_op = match self.head {
            None => SurfaceOp::Append,
            Some(head) => SurfaceOp::Replace {
                start_seq: head,
                end_seq: head,
            },
        };
        let mut event = Event {
            type_: "system/message".to_owned(),
            seq,
            time: anchor_time,
            data,
            ignorable: None,
            source_event_seqs: self.head.map(|head| vec![head]),
            surface_op: Some(surface_op),
        };
        canonicalize(&mut event);
        self.events.push(event);
        self.head = Some(seq);
        prompt.clone_into(&mut self.prompt);
        Ok(())
    }
}

/// The `id` of a message record, which every surface message carries.
fn message_id(message: &Map<String, Value>) -> Result<String, MigrationError> {
    message
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| MigrationError::format("message id must be a string"))
}

/// Migrate a complete released-v2 artifact.
///
/// `source_inherited_event_count` follows [`V2ToV3::new`]: [`None`] lets a seeded log derive its cut
/// from its marker, which is what a whole-artifact caller wants.
pub fn migrate_session(
    header: &Header,
    events: Vec<Event>,
    source_inherited_event_count: Option<usize>,
) -> Result<SessionArtifact, MigrationError> {
    let mut stage = V2ToV3::new(header, source_inherited_event_count)?;
    stage.transform_events(events)?;
    stage.into_artifact()
}

/// The stable inventory of event types released v2 admits, for callers that need it.
#[must_use]
pub fn admitted_v2_types() -> Vec<&'static str> {
    let mut types: Vec<&'static str> = V2_EVENT_DISPOSITIONS
        .iter()
        .map(|(name, _, _)| *name)
        .collect();
    types.extend_from_slice(&["feedback/message-put", "feedback/message-delete"]);
    types.sort_unstable();
    types
}

#[cfg(test)]
mod inline {
    use super::*;

    /// The digest has to be *this* SHA-256, not merely a hash: a generated id is compared against
    /// logs migrated by upstream, so a wrong digest would look like a different message.
    #[test]
    fn sha256_matches_published_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"The quick brown fox jumps over the lazy dog"),
            "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
        );
        // A second block, so the padding branch that spans a chunk boundary is exercised.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn inventory_is_sorted_and_complete() {
        let types = admitted_v2_types();
        assert_eq!(
            types.len(),
            53,
            "51 frozen dispositions plus the two feedback events"
        );
        assert!(types.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(types.contains(&"assistant/message"));
        // v1 folded these away, so v2 must not admit them.
        assert!(!types.contains(&"assistant/chunk"));
    }

    #[test]
    fn generated_identity_is_the_upstream_value() {
        let identity = serde_json::to_string(&Value::Array(vec![
            Value::String(MIGRATION_NAME.to_owned()),
            Value::String("identity".to_owned()),
            Value::from(1_usize),
            Value::String("step/start".to_owned()),
        ]))
        .expect("identity encodes");
        assert_eq!(
            identity,
            r#"["session-format-v2-to-v3","identity",1,"step/start"]"#
        );
        assert_eq!(
            format!("{GENERATED_ID_PREFIX}{}", sha256_hex(identity.as_bytes())),
            "v2-to-v3-system-c43ced201acd9ac5b45b8d69daae84defd1d9d121ec07bd370ba3babb91ca20d"
        );
    }
}
