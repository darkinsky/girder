//! Recognizing a session log's format generation and upgrading old rows to the current one.
//!
//! Upstream ships four adjacent migration packages — `v0-to-v1`, `v1-to-v2`, `v2-to-v3`, and
//! `v3-to-v4` — each turning one released generation of the session JSONL into the next. A
//! rewritten harness cannot read an old log without them: the reader's types describe *today's*
//! vocabulary, and a log written two generations ago names events and nests payloads that no
//! longer decode. This module is the seam that recovers such a log.
//!
//! # What each upstream generation changed
//!
//! | edge | change |
//! |---|---|
//! | v0 → v1 | Identity conversion of the shared layout plus a finite set of legacy normalizations: `steering/message` becomes `user/message`; `compact/*` becomes `compaction/*`; `turn/start.trigger` is dropped; retired `turn/end` reasons (`disposed`, bare `aborted`, flattened `error`) become the current tags; messages without a wrapper gain `id`/`role`/`source` (ids are `legacy-message:<session>:<seq>`); retry and compaction groups gain deterministic ids; `request/header.header.messagePrefix` is dropped; the retired `request/header-delta` and `mode/set` events refuse migration. No event is added or removed. |
//! | v1 → v2 | Cardinality-changing: top-level `assistant/chunk` rows fold into the matching `assistant/message` as an embedded `data.stream`, an unclaimed attempt becomes a log-only `assistant/attempt`, and every surviving event plus every declared same-session sequence reference is renumbered densely. A seeded log gains a tagged `session/end-seed` marker that fixes the inherited cut. |
//! | v2 → v3 | The system prompt stops riding `request/header` and becomes a protected surface head: an empty `system/message` follows the first `step/start`, later prompt changes are inserted as replacements, and `header.system` disappears. Envelopes are canonicalized (`surfaceOp.replace` gains `startSeq`/`endSeq`; empty `tools`/`adapterDefaults` are omitted), the `code` preset is renamed `ptc`, and `tool/code-dispatch*` becomes `tool/ptc-dispatch*` with `tools-code-mode` attribution renamed `tools-ptc`. Insertions shift every later sequence number. |
//! | v3 → v4 | `tool/result` stops hiding behind a `user`-role message with a `tool-result` wrapper and becomes a first-class `tool`-role message; plugin message sources become producer kinds (`compact` → `compact-checkpoint`, `tools-ptc` → `ptc-mode`, anything unknown → `plugin:<name>`); unknown content tags become `plugin:<type>`; unknown *ignorable* event types are namespaced; an interrupted turn is closed with `turn/end { reason: interrupted }`; missing `subagent/catalog` facts are appended. |
//!
//! # What this module migrates, and what it refuses
//!
//! It migrates **one row at a time**, because that is what a JSONL reader has in hand when it hits
//! a line it cannot decode. That deliberately excludes two edges, and the module refuses rather
//! than guesses:
//!
//! | edge | status | why |
//! |---|---|---|
//! | v0 → v1 | supported | every transformation is row-local, except that ids are minted from the session id, which [`upgrade_in_session`] supplies |
//! | v1 → v2 | [`MigrationError::Unsupported`] | folding chunks into messages and renumbering densely is whole-artifact work; a row cannot know its position |
//! | v2 → v3 | [`MigrationError::Unsupported`] | a row upgrade cannot insert the system head or the prompt replacements, and an output missing its protected head is not a valid v3 log |
//! | v3 → v4 | supported, minus its insertions | the row-local conversions — result lift, producer kinds, content namespacing — are the bulk of the edge; the interrupted-turn repair and appended catalog facts are insertions that need surrounding rows |
//!
//! Even the supported edges refuse individual rows when the correct result depends on state this
//! module cannot see: an open compaction id, a retry chain, a replacement's inherited message
//! identity, or a packed chunk run. Those cases name exactly what is missing, so a caller can
//! decide whether to migrate the artifact properly instead of accepting a plausible-looking wrong
//! answer.
//!
//! # Why detection runs newest-first
//!
//! The newest generation's markers are the most specific and the oldest generation's are the most
//! permissive: a v4 row is still a `data`-enveloped event with a known type, so probing for v0
//! first would classify nearly everything as v0. Detection therefore tests v4, then v3, then v2,
//! then the retired v0 spellings, and only then falls back to v1 — the generation any unremarkable
//! enveloped row belongs to, because a v0 row that v0→v1 leaves untouched is byte-identical to v1
//! and upgrading it is a no-op either way.
//!
//! # Detection is evidence, not a verdict
//!
//! A generation-neutral row — `step/start`, a completed `turn/end` — is written identically by v1,
//! v2, and v3, so no row-local probe can tell them apart; only the header's `version` can. [`detect`]
//! therefore reports the *most specific* generation the row evidences, and [`Migrator::upgrade`]
//! applies the caller's edge when the row does not evidence anything newer. The one exception is a
//! row pinned to v0: those spellings were renamed by v0→v1 and cannot appear in a later log, so
//! claiming a later generation for one is a skipped edge and is reported as
//! [`MigrationError::VersionMismatch`].
//!
//! # The current generation
//!
//! This build writes a *reduced* envelope: the event tag is the top-level `type`, with no `seq`,
//! `time`, or `data` wrapper (see `crate::session::SessionEvent`). A row in that shape is reported
//! as [`FormatVersion::V4`] — it is what the installed reader accepts, so there is nothing above it
//! to migrate to — and [`upgrade`] returns it unchanged. The v3→v4 edge below targets *upstream* v3
//! rows, which is what importing another harness's session files requires.
//!
//! # Reference: what a row looks like in each generation
//!
//! ```text
//! v0  {"type":"steering/message","seq":3,"time":1,"data":{"turn":1,"content":[],"source":{}}}
//! v1  {"type":"user/message","seq":3,"time":1,"data":{"id":"m1","role":"user","content":[],"source":{}}}
//! v2  {"type":"assistant/message","seq":4,"time":2,"data":{"message":{},"stream":[]}}
//! v3  {"type":"tool/result","seq":5,"time":3,"data":{"message":{"role":"user","content":[{"type":"tool-result"}]}}}
//! v4  {"type":"tool/result","seq":5,"time":3,"data":{"message":{"role":"tool","toolCallId":"c1"}}}
//! ```

use serde_json::{Map, Value};

/// One row's location in the format chain.
///
/// Ordered, so "is this row newer than the generation I was told to migrate" is a comparison rather
/// than a table lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FormatVersion {
    /// The released lane before the shared layout: `steering/message`, `compact/*`, flat
    /// `tool/result`, `turn/start.trigger`.
    V0,
    /// The shared layout: wrapped messages carrying `id`/`role`/`source`, `sourceEventSeqs`, and
    /// `surfaceOp.replace { start, end }`.
    V1,
    /// Embedded assistant streams, `assistant/attempt`, and a tagged inherited cut.
    V2,
    /// A system-prompt surface head, canonical `startSeq`/`endSeq` envelopes, and PTC naming.
    V3,
    /// First-class `tool`-role results, producer kinds instead of plugin wrappers, and namespaced
    /// extension tags. This is the generation the installed reader understands.
    V4,
}

impl FormatVersion {
    /// The generation this build reads.
    pub const CURRENT: Self = Self::V4;

    /// Every generation, oldest first.
    pub const ALL: [Self; 5] = [Self::V0, Self::V1, Self::V2, Self::V3, Self::V4];

    /// The generation a numeric `version` field names, when it names one this build knows.
    ///
    /// A higher number is deliberately not clamped to [`Self::CURRENT`]: a log written by a newer
    /// harness must be reported as unrecognized rather than silently treated as current.
    #[must_use]
    pub const fn from_number(version: u64) -> Option<Self> {
        match version {
            0 => Some(Self::V0),
            1 => Some(Self::V1),
            2 => Some(Self::V2),
            3 => Some(Self::V3),
            4 => Some(Self::V4),
            _ => None,
        }
    }

    /// The generation this one upgrades to, or `None` for the current one.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self {
            Self::V0 => Some(Self::V1),
            Self::V1 => Some(Self::V2),
            Self::V2 => Some(Self::V3),
            Self::V3 => Some(Self::V4),
            Self::V4 => None,
        }
    }

    /// Whether this is the generation the installed reader understands.
    #[must_use]
    pub const fn is_current(self) -> bool {
        matches!(self, Self::CURRENT)
    }

    /// The number the on-disk `version` field carries.
    #[must_use]
    pub const fn as_number(self) -> u64 {
        match self {
            Self::V0 => 0,
            Self::V1 => 1,
            Self::V2 => 2,
            Self::V3 => 3,
            Self::V4 => 4,
        }
    }
}

impl std::fmt::Display for FormatVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "v{}", self.as_number())
    }
}

/// One adjacent edge of the migration chain.
///
/// The framework describes all four edges even though only two are implemented, so a caller can ask
/// what the chain contains and which parts of it this build can perform instead of discovering a
/// gap by trying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Migration {
    /// The generation the edge reads.
    pub from: FormatVersion,
    /// The generation the edge writes.
    pub to: FormatVersion,
}

impl Migration {
    /// Builds an edge.
    ///
    /// Adjacency is not enforced here: [`Self::edges`] is the only place that needs it, and
    /// [`Migrator::upgrade`] rejects a non-adjacent edge by construction.
    #[must_use]
    pub const fn new(from: FormatVersion, to: FormatVersion) -> Self {
        Self { from, to }
    }

    /// The four adjacent edges, oldest first.
    #[must_use]
    pub const fn edges() -> [Self; 4] {
        [
            Self::new(FormatVersion::V0, FormatVersion::V1),
            Self::new(FormatVersion::V1, FormatVersion::V2),
            Self::new(FormatVersion::V2, FormatVersion::V3),
            Self::new(FormatVersion::V3, FormatVersion::V4),
        ]
    }

    /// Whether this build can perform the edge for a single row.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(
            (self.from, self.to),
            (FormatVersion::V0, FormatVersion::V1) | (FormatVersion::V3, FormatVersion::V4)
        )
    }

    /// Why this build cannot perform the edge, or `None` when it can.
    ///
    /// An unsupported edge is not merely absent; it needs information a row does not carry. Saying
    /// which information is the difference between a caller reaching for the artifact path and a
    /// caller concluding the log is unreadable.
    #[must_use]
    pub const fn unsupported_reason(self) -> Option<&'static str> {
        match (self.from, self.to) {
            (FormatVersion::V0, FormatVersion::V1) | (FormatVersion::V3, FormatVersion::V4) => None,
            (FormatVersion::V1, FormatVersion::V2) => Some(
                "v1-to-v2 folds assistant chunk rows into their message and renumbers every later \
                 event densely, which needs the whole artifact rather than one row",
            ),
            (FormatVersion::V2, FormatVersion::V3) => Some(
                "v2-to-v3 inserts the system-prompt head and its replacements, so a row-at-a-time \
                 upgrade would emit a v3 log without its protected head",
            ),
            _ => Some("the edge is not part of the adjacent chain"),
        }
    }
}

/// A failure to migrate one row.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// The line is not JSON, or not a JSON object.
    #[error("session row is not a JSON object: {detail}")]
    Malformed {
        /// What the parser or the shape check reported.
        detail: String,
    },

    /// The line is JSON but violates the claimed generation's rules.
    #[error("{version} session row is invalid: {detail}")]
    Invalid {
        /// The generation whose rules the row violates.
        version: FormatVersion,
        /// The violated rule, in terms a log reader can act on.
        detail: String,
    },

    /// The edge, or this particular row within it, cannot be migrated faithfully.
    #[error("cannot migrate {from} to {to}: {reason}")]
    Unsupported {
        /// The generation the row claims.
        from: FormatVersion,
        /// The generation the caller asked for.
        to: FormatVersion,
        /// What would have to be known or done, so a caller can decide what to do instead.
        reason: String,
    },

    /// The line is older than the caller claimed, so the requested edge would skip a generation.
    #[error("session row is {detected}, but the caller asked to migrate it from {claimed}")]
    VersionMismatch {
        /// The generation the caller named.
        claimed: FormatVersion,
        /// The generation the row evidences.
        detected: FormatVersion,
    },

    /// The line does not look like any generation this build knows.
    #[error("session row matches no known format version, so it cannot be migrated from {from}")]
    Unrecognized {
        /// The generation the caller named.
        from: FormatVersion,
    },
}

/// A row-level migrator that can mint identities when it knows the session they belong to.
///
/// [`upgrade`] is the stateless form and refuses the rows that need a session id. This type exists
/// because v0 minted deterministic message ids from the session id, which a bare row does not carry;
/// a caller that has already read the header can supply it and migrate those rows too.
#[derive(Clone, Copy, Debug, Default)]
pub struct Migrator<'a> {
    /// The session id from the header, when the caller has read it.
    session: Option<&'a str>,
}

impl<'a> Migrator<'a> {
    /// A migrator without session identity; rows needing a minted id are refused.
    #[must_use]
    pub const fn new() -> Self {
        Self { session: None }
    }

    /// A migrator for a session whose header id is already known.
    #[must_use]
    pub const fn in_session(session: &'a str) -> Self {
        Self {
            session: Some(session),
        }
    }

    /// Upgrades one row from `from` to the next generation.
    ///
    /// A row that evidences a *newer* generation than `from` is returned byte-for-byte unchanged,
    /// which is what makes the operation idempotent: re-running a migration over a log that was
    /// partly migrated already leaves the migrated rows alone.
    pub fn upgrade(&self, line: &str, from: FormatVersion) -> Result<String, MigrationError> {
        let detected = detect(line).ok_or(MigrationError::Unrecognized { from })?;
        if detected > from {
            return Ok(line.to_owned());
        }
        // A v0 spelling was renamed by v0→v1, so it cannot appear in a later generation's log:
        // claiming one proves the caller skipped that edge. Every other "older" verdict is a
        // generation-neutral row — v2 introduced streams and attempts that v3 kept, for instance —
        // so it is compatible with the claimed generation and the edge below converts it.
        if detected == FormatVersion::V0 && from != FormatVersion::V0 {
            return Err(MigrationError::VersionMismatch {
                claimed: from,
                detected,
            });
        }
        let Some(to) = from.next() else {
            return Ok(line.to_owned());
        };

        let mut value: Value =
            serde_json::from_str(line).map_err(|error| MigrationError::Malformed {
                detail: error.to_string(),
            })?;
        let root = value
            .as_object_mut()
            .ok_or_else(|| MigrationError::Malformed {
                detail: "the parsed value is not an object".to_owned(),
            })?;

        match (from, to) {
            (FormatVersion::V0, FormatVersion::V1) => migrate_v0_to_v1(root, self.session)?,
            (FormatVersion::V3, FormatVersion::V4) => migrate_v3_to_v4(root)?,
            _ => {
                let edge = Migration::new(from, to);
                return Err(MigrationError::Unsupported {
                    from,
                    to,
                    reason: edge
                        .unsupported_reason()
                        .unwrap_or("the edge is not implemented")
                        .to_owned(),
                });
            }
        }

        serde_json::to_string(&value).map_err(|error| MigrationError::Malformed {
            detail: error.to_string(),
        })
    }
}

/// Detects which generation a single JSONL row belongs to.
///
/// Returns `None` for anything this build cannot place: a line that is not JSON, a value that is not
/// an object, an object without a `type`, or a header claiming a generation newer than v4. A `None`
/// is a refusal to guess, not an error in itself.
#[must_use]
pub fn detect(line: &str) -> Option<FormatVersion> {
    let value: Value = serde_json::from_str(line).ok()?;
    let root = value.as_object()?;
    let tag = root.get("type")?.as_str()?;

    if tag == "session" {
        return root
            .get("version")
            .and_then(Value::as_u64)
            .and_then(FormatVersion::from_number);
    }
    if root.contains_key("seq0") {
        // A packed chunk run, which both physical codecs write and the v0→v1 edge passes through
        // untouched. It is checked before the envelope test because it frames several events with
        // `seq0`/`time0` instead of one with `seq`/`time`.
        return Some(FormatVersion::V1);
    }
    if has_envelope_markers(root) {
        // A row that started to grow an envelope but did not finish it is corrupt in every
        // generation; calling it current would let a truncated write through as a valid event.
        if !has_envelope(root) {
            return None;
        }
    } else {
        // The installed reader's reduced row: no `seq`/`time`/`data` at all. Only its own
        // vocabulary is accepted, so an arbitrary object carrying a `type` field is not silently
        // called current.
        return CURRENT_EVENT_TYPES
            .contains(&tag)
            .then_some(FormatVersion::V4);
    }
    Some(detect_enveloped(tag, root))
}

/// Upgrades one row to the next generation, without session identity.
///
/// Rows whose correct migration needs the session id from the header are refused with
/// [`MigrationError::Unsupported`]; use [`upgrade_in_session`] for those.
pub fn upgrade(line: &str, from: FormatVersion) -> Result<String, MigrationError> {
    Migrator::new().upgrade(line, from)
}

/// Upgrades one row, minting legacy identities from the id the session header carries.
pub fn upgrade_in_session(
    line: &str,
    from: FormatVersion,
    session: &str,
) -> Result<String, MigrationError> {
    Migrator::in_session(session).upgrade(line, from)
}

// ---- detection vocabulary ---------------------------------------------------------------------

/// The event tags the installed reader (`crate::session::SessionEvent`) understands.
const CURRENT_EVENT_TYPES: &[&str] = &[
    "assistant/message",
    "developer/message",
    "llm/retry",
    "model/selection",
    "model/usage",
    "permission/preset",
    "plan/mode",
    "request/context",
    "sandbox/mode",
    "session/title",
    "step/end",
    "step/start",
    "system/message",
    "tool/call",
    "tool/result",
    "turn/end",
    "turn/start",
    "user/message",
];

/// The frozen event inventory of released v3, as `v3-to-v4` declares it.
///
/// Copied rather than derived from the current writer on purpose: a v3 reader must classify *v3*
/// rows, and a list that grew with this build would accept an event v3 never wrote.
const RELEASED_V3_EVENT_TYPES: &[&str] = &[
    "agent-preset/selected",
    "agent/inbox/spliced",
    "approval/asked",
    "approval/decided",
    "approval/policy",
    "assistant/attempt",
    "assistant/message",
    "command/done",
    "command/run",
    "compaction/end",
    "compaction/prune",
    "compaction/start",
    "compaction/summary",
    "deliverables/presented",
    "feedback/message-delete",
    "feedback/message-put",
    "feedback/record",
    "goal/change",
    "hook/invoked",
    "hook/result",
    "image/offload",
    "llm/retry",
    "llm/retry-started",
    "model/selection",
    "permission/preset",
    "plan/mode",
    "request/context",
    "request/header",
    "sandbox/mode",
    "schedule/change",
    "session-log-deepseek/delivery-accepted",
    "session/end-seed",
    "session/title",
    "session/title-llm-request",
    "step/end",
    "step/start",
    "subagent/catalog",
    "subagent/descriptor",
    "subagent/model-selection-policy",
    "system/message",
    "team/member",
    "team/message/delivered",
    "team/message/queued",
    "team/task",
    "todo/write",
    "tool-workflow/agent-end",
    "tool-workflow/agent-start",
    "tool-workflow/run-end",
    "tool-workflow/run-start",
    "tool/call",
    "tool/ptc-dispatch",
    "tool/ptc-dispatch-start",
    "tool/result",
    "turn/end",
    "turn/start",
    "user/message",
    "web/deepseek-search-llm-request",
    "workspace/changes",
];

/// Event types v2 could not write, so their presence proves a row is v3 or newer.
const V3_ONLY_EVENT_TYPES: &[&str] = &[
    "system/message",
    "tool/ptc-dispatch",
    "tool/ptc-dispatch-start",
];

/// Event types v1 could not write, so their presence proves a row is v2 or newer.
const V2_ONLY_EVENT_TYPES: &[&str] = &["assistant/attempt"];

/// Event types only the v0 lane wrote, which v0→v1 renames or refuses.
///
/// Because v0→v1 removes every one of these names, a row carrying one cannot belong to a later
/// generation; that is what makes the v0 verdict exact rather than merely oldest.
const V0_ONLY_EVENT_TYPES: &[&str] = &[
    "compact/end",
    "compact/prune",
    "compact/start",
    "compact/summary",
    "mode/set",
    "request/header-delta",
    "steering/message",
];

/// The content block tags v3 admitted; anything else was an extension.
const CONTENT_KINDS: &[&str] = &[
    "text",
    "reasoning",
    "image",
    "file",
    "tool-call",
    "tool-result",
];

/// Plugin names whose v4 producer kind is the plugin string itself.
const SAME_NAME_PRODUCERS: &[&str] = &[
    "agent-instructions",
    "agent-message",
    "coordinator",
    "cordis-host-runner",
    "dsh-session-title-llm",
    "goal",
    "hooks-claude-code",
    "hooks-codex",
    "model-selection",
    "plan-mode",
    "repeat-tool-reminder",
    "schedule",
    "session-reference",
    "skill-catalog",
    "skill-invocation",
    "subagent-report",
    "subagent-settled",
    "team-message",
    "time-context",
    "tmux-context",
    "tool-cordis",
    "tool-goal",
    "tool-jobs",
    "user-approval",
    "webhook",
];

/// Producer kinds a v3 row never carried directly, so seeing one proves v3→v4 already ran.
const V4_SOURCE_KINDS: &[&str] = &[
    "compact-basic",
    "compact-checkpoint",
    "ptc-mode",
    "runtime-context",
    "system-prompt",
];

fn has_envelope_markers(root: &Map<String, Value>) -> bool {
    root.contains_key("seq") || root.contains_key("time") || root.contains_key("data")
}

/// Whether the row carries a complete, well-formed v1-and-later envelope.
fn has_envelope(root: &Map<String, Value>) -> bool {
    // An integer `seq` is an invariant of every generation, so a row without one is not a session
    // event at all — reporting a generation for it would invite an upgrade that must fail anyway.
    root.get("seq").is_some_and(Value::is_u64)
        && root.contains_key("time")
        && root.contains_key("data")
}

/// Places an enveloped, non-header row.
///
/// The probes run newest-first; see the module documentation for why that order is not optional.
fn detect_enveloped(tag: &str, root: &Map<String, Value>) -> FormatVersion {
    let data = root.get("data").and_then(Value::as_object);
    if is_v4_row(tag, data) {
        return FormatVersion::V4;
    }
    if is_v3_row(tag, data) {
        return FormatVersion::V3;
    }
    if is_v2_row(tag, data) {
        return FormatVersion::V2;
    }
    if is_v0_row(tag, data) {
        return FormatVersion::V0;
    }
    FormatVersion::V1
}

/// Markers that only a v3→v4 conversion produces.
fn is_v4_row(tag: &str, data: Option<&Map<String, Value>>) -> bool {
    if tag == "developer/message" || tag.starts_with("plugin:") {
        return true;
    }
    let Some(data) = data else {
        return false;
    };
    if tag == "tool/result"
        && message_of(data)
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("tool")
    {
        return true;
    }
    if messages(tag, data).into_iter().any(has_v4_message_marker) {
        return true;
    }
    has_namespaced_content(tag, data)
}

/// Whether one message already carries v3→v4 output rather than v3 input.
///
/// Content namespacing is included because it is the marker that keeps [`Migrator::upgrade`]
/// idempotent for tags like `plugin:foo`: without it a second run would prefix the tag again.
fn has_v4_message_marker(message: &Map<String, Value>) -> bool {
    let kind = message
        .get("source")
        .and_then(Value::as_object)
        .and_then(|source| source.get("kind"))
        .and_then(Value::as_str);
    let converted_kind = kind.is_some_and(|kind| {
        kind.starts_with("plugin:")
            || V4_SOURCE_KINDS.contains(&kind)
            || SAME_NAME_PRODUCERS.contains(&kind)
    });
    converted_kind || has_namespaced_blocks(message.get("content"))
}

/// Whether a message's content is one `tool-result` wrapper block.
fn holds_tool_result_wrapper(message: &Map<String, Value>) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .filter(|blocks| blocks.len() == 1)
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("type"))
        .and_then(Value::as_str)
        == Some("tool-result")
}

/// Markers that only the v2→v3 conversion produces.
fn is_v3_row(tag: &str, data: Option<&Map<String, Value>>) -> bool {
    if V3_ONLY_EVENT_TYPES.contains(&tag) {
        return true;
    }
    let Some(data) = data else {
        return false;
    };
    if tag == "agent-preset/selected"
        && data.get("agentPreset").and_then(Value::as_str) == Some("ptc")
    {
        return true;
    }
    // A v3 tool result is a `user`-role message holding exactly one `tool-result` wrapper. v4 lifted
    // that into a `tool`-role message, so the wrapper is the marker that tells the two apart.
    if tag == "tool/result"
        && message_of(data)
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("user")
        && message_of(data).is_some_and(holds_tool_result_wrapper)
    {
        return true;
    }
    // `startSeq`/`endSeq` is the canonical replacement spelling; `start`/`end` is pre-v3.
    data.get("surfaceOp")
        .and_then(Value::as_object)
        .is_some_and(|op| op.contains_key("startSeq") || op.contains_key("endSeq"))
}

/// Whether the row still uses the pre-v3 replacement spelling.
///
/// `surfaceOp.replace { start, end }` was renamed to `startSeq`/`endSeq` by v2→v3, so a row carrying
/// the old names has not run that edge.
fn is_legacy_replacement(data: &Map<String, Value>) -> bool {
    data.get("surfaceOp")
        .and_then(Value::as_object)
        .is_some_and(|op| {
            op.get("op").and_then(Value::as_str) == Some("replace")
                && !op.contains_key("startSeq")
                && !op.contains_key("endSeq")
                && (op.contains_key("start") || op.contains_key("end"))
        })
}

/// Markers that only the v1→v2 conversion produces.
fn is_v2_row(tag: &str, data: Option<&Map<String, Value>>) -> bool {
    if V2_ONLY_EVENT_TYPES.contains(&tag) {
        return true;
    }
    let Some(data) = data else {
        return false;
    };
    if tag == "assistant/message" && data.get("stream").is_some_and(Value::is_array) {
        return true;
    }
    tag == "session/end-seed" && data.get("inherited").and_then(Value::as_bool) == Some(true)
}

/// Markers that only the v0 lane wrote.
///
/// Every row v0→v1 changes is identifiable this way, which is why the v1 fallback cannot swallow a
/// row that still needs the v0 edge.
fn is_v0_row(tag: &str, data: Option<&Map<String, Value>>) -> bool {
    if V0_ONLY_EVENT_TYPES.contains(&tag) {
        return true;
    }
    let Some(data) = data else {
        return false;
    };
    match tag {
        "turn/start" => data.contains_key("trigger"),
        "turn/end" => data
            .get("reason")
            .and_then(Value::as_object)
            .is_some_and(is_legacy_turn_end_reason),
        "request/header" => {
            data.get("reason").and_then(Value::as_str) == Some("fallback")
                || data
                    .get("header")
                    .and_then(Value::as_object)
                    .is_some_and(|header| header.contains_key("messagePrefix"))
        }
        "user/message" => is_legacy_message_shape(data),
        "assistant/message" => {
            data.contains_key("content")
                && data.contains_key("provenance")
                && !data.contains_key("message")
        }
        "tool/result" => {
            data.contains_key("callId")
                && data.contains_key("content")
                && !data.contains_key("message")
        }
        "llm/retry" => !data.contains_key("retryId"),
        "compaction/start" | "compaction/summary" | "compaction/end" => {
            !data.contains_key("compactionId")
        }
        _ => false,
    }
}

/// Whether a `turn/end.reason` still uses a retired v0 tag.
fn is_legacy_turn_end_reason(reason: &Map<String, Value>) -> bool {
    match reason.get("kind").and_then(Value::as_str) {
        Some("disposed") => true,
        Some("aborted") => !reason.contains_key("reason"),
        Some("error") => !reason.contains_key("error"),
        _ => false,
    }
}

/// Whether a message payload is the flat v0 shape, before `v0-to-v1` wrapped it.
fn is_legacy_message_shape(data: &Map<String, Value>) -> bool {
    data.contains_key("content")
        && data.contains_key("source")
        && !data.contains_key("id")
        && !data.contains_key("role")
        && !data.contains_key("message")
}

fn invalid(version: FormatVersion, detail: impl Into<String>) -> MigrationError {
    MigrationError::Invalid {
        version,
        detail: detail.into(),
    }
}

fn unsupported(
    from: FormatVersion,
    to: FormatVersion,
    reason: impl Into<String>,
) -> MigrationError {
    MigrationError::Unsupported {
        from,
        to,
        reason: reason.into(),
    }
}

/// The event's message, for the four tags that nest one under `data.message`.
fn message_of(data: &Map<String, Value>) -> Option<&Map<String, Value>> {
    data.get("message").and_then(Value::as_object)
}

/// Every message an event carries at a declared position.
///
/// Only the positions the format defines are walked: a message-shaped object inside tool arguments
/// or replay state is data, not history, and rewriting it would corrupt what the model was shown.
fn messages<'a>(tag: &str, data: &'a Map<String, Value>) -> Vec<&'a Map<String, Value>> {
    match tag {
        "user/message" => vec![data],
        "assistant/message" | "developer/message" | "system/message" | "tool/result" => {
            message_of(data).into_iter().collect()
        }
        "agent/inbox/spliced" => array_messages(data, "inserted"),
        "session/title-llm-request" => array_messages(data, "messages"),
        _ => Vec::new(),
    }
}

fn array_messages<'a>(data: &'a Map<String, Value>, key: &str) -> Vec<&'a Map<String, Value>> {
    data.get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default()
}

/// Whether a content array holds a block whose tag v3→v4 already namespaced.
fn has_namespaced_blocks(value: Option<&Value>) -> bool {
    value.and_then(Value::as_array).is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.starts_with("plugin:"))
        })
    })
}

/// Whether an event's declared content positions hold a namespaced tag.
///
/// Separate from [`has_v4_message_marker`] because three of the converted positions are not
/// messages: compaction summaries, PTC output, and embedded stream chunks.
fn has_namespaced_content(tag: &str, data: &Map<String, Value>) -> bool {
    match tag {
        "compaction/summary" => {
            has_namespaced_blocks(data.get("summary"))
                || has_namespaced_blocks(data.get("rawOutput"))
        }
        "tool/ptc-dispatch" => has_namespaced_blocks(data.get("content")),
        "assistant/message" | "assistant/attempt" => data
            .get("stream")
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    let Some(chunk) = entry.get("chunk").and_then(Value::as_object) else {
                        return false;
                    };
                    match chunk.get("type").and_then(Value::as_str) {
                        Some("block-end") => chunk
                            .get("block")
                            .and_then(Value::as_object)
                            .and_then(|block| block.get("type"))
                            .and_then(Value::as_str)
                            .is_some_and(|kind| kind.starts_with("plugin:")),
                        Some("block-start") => chunk
                            .get("blockType")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| kind.starts_with("plugin:")),
                        _ => false,
                    }
                })
            }),
        _ => false,
    }
}

fn string_field<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

/// The row's `data`, mutably.
fn data_object_mut(
    root: &mut Map<String, Value>,
) -> Result<&mut Map<String, Value>, MigrationError> {
    root.get_mut("data")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| MigrationError::Malformed {
            detail: "the row has no object `data` member".to_owned(),
        })
}

/// The row's `seq`, which every minted legacy id is derived from.
fn seq_of(root: &Map<String, Value>) -> Result<u64, MigrationError> {
    root.get("seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| MigrationError::Malformed {
            detail: "the row has no integer `seq` member".to_owned(),
        })
}

// ---- v0 -> v1 ---------------------------------------------------------------------------------

/// Promotes one released-v0 row into the shared layout.
fn migrate_v0_to_v1(
    root: &mut Map<String, Value>,
    session: Option<&str>,
) -> Result<(), MigrationError> {
    let tag = string_field(root, "type")
        .ok_or_else(|| invalid(FormatVersion::V0, "row lacks a string `type`"))?
        .to_owned();

    if tag == "session" {
        return migrate_v0_header(root);
    }
    if tag == "request/header-delta" || tag == "mode/set" {
        return Err(unsupported(
            FormatVersion::V0,
            FormatVersion::V1,
            format!("the retired `{tag}` event has no current equivalent"),
        ));
    }

    // The tag is renamed before the payload is touched so every later step sees one spelling.
    let normalized = match tag.as_str() {
        "steering/message" => "user/message",
        "compact/start" => "compaction/start",
        "compact/summary" => "compaction/summary",
        "compact/end" => "compaction/end",
        "compact/prune" => "compaction/prune",
        other => other,
    };
    if normalized != tag {
        root.insert("type".to_owned(), Value::from(normalized));
    }

    match normalized {
        "turn/start" => normalize_turn_start(root),
        "turn/end" => normalize_turn_end(root),
        "request/header" => normalize_request_header(root),
        "llm/retry" => normalize_retry(root, session),
        "compaction/start" => normalize_compaction_start(root, session),
        "compaction/summary" | "compaction/end" => Err(unsupported(
            FormatVersion::V0,
            FormatVersion::V1,
            "this compaction member has no `compactionId` of its own, so it needs the id minted \
             by its opening `compaction/start`; migrate the artifact in order rather than this row \
             alone",
        )),
        "user/message" => normalize_user_message(root, session),
        "assistant/message" => normalize_assistant_message(root, session),
        "tool/result" => normalize_tool_result(root, session),
        _ => Ok(()),
    }
}

/// Advances the physical header. `seedLength` stays: it is framing, not the logical `isSeeded`.
fn migrate_v0_header(root: &mut Map<String, Value>) -> Result<(), MigrationError> {
    if root.get("version").and_then(Value::as_u64) != Some(0) {
        return Err(invalid(
            FormatVersion::V0,
            "header does not carry `version: 0`",
        ));
    }
    for key in ["id", "createdAt", "delegationDepth"] {
        if !root.contains_key(key) {
            return Err(invalid(FormatVersion::V0, format!("header lacks `{key}`")));
        }
    }
    root.insert("version".to_owned(), Value::from(1));
    Ok(())
}

fn normalize_turn_start(root: &mut Map<String, Value>) -> Result<(), MigrationError> {
    let data = data_object_mut(root)?;
    let Some(trigger) = data.get("trigger") else {
        return Ok(());
    };
    let trigger_kind = trigger
        .get("kind")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty());
    if trigger_kind.is_none() {
        return Err(invalid(
            FormatVersion::V0,
            "`turn/start.trigger` must carry a nonempty `kind`",
        ));
    }
    let turn = data
        .get("turn")
        .and_then(Value::as_u64)
        .filter(|turn| *turn >= 1)
        .ok_or_else(|| invalid(FormatVersion::V0, "`turn/start.turn` must be at least 1"))?;
    // The trigger recorded why the turn began; v1 derives that from the turn itself, so keeping the
    // field would leave a v1 log carrying a member v1 never wrote.
    let mut replacement = Map::new();
    replacement.insert("turn".to_owned(), Value::from(turn));
    *data = replacement;
    Ok(())
}

fn normalize_turn_end(root: &mut Map<String, Value>) -> Result<(), MigrationError> {
    let turn = {
        let data = data_object_mut(root)?;
        data.get("turn")
            .and_then(Value::as_u64)
            .filter(|turn| *turn >= 1)
            .ok_or_else(|| invalid(FormatVersion::V0, "`turn/end.turn` must be at least 1"))?
    };
    let current = {
        let data = data_object_mut(root)?;
        let reason = data
            .get("reason")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid(FormatVersion::V0, "`turn/end.reason` must be an object"))?;
        let kind = string_field(reason, "kind")
            .ok_or_else(|| invalid(FormatVersion::V0, "`turn/end.reason.kind` must be a string"))?;
        // Only the retired spellings are rewritten. A kind this build does not interpret is
        // owner-opaque — another producer's reason must keep its recorded meaning, so it falls
        // through to the same no-op as the tags that are already current.
        match kind {
            // A bare `aborted` was a cancellation whose cause was never recorded; `disposed` was
            // the session shutting down. Both are a cancellation today, told apart by the cause.
            "aborted" if !reason.contains_key("reason") => {
                Some(serde_json::json!({ "kind": "aborted", "reason": { "kind": "legacy" } }))
            }
            "disposed" => {
                Some(serde_json::json!({ "kind": "aborted", "reason": { "kind": "disposed" } }))
            }
            "error" if !reason.contains_key("error") => Some(legacy_error_reason(reason)?),
            _ => None,
        }
    };
    let Some(current) = current else {
        return Ok(());
    };

    let data = data_object_mut(root)?;
    let mut replacement = Map::new();
    replacement.insert("turn".to_owned(), Value::from(turn));
    replacement.insert("reason".to_owned(), current);
    *data = replacement;
    Ok(())
}

/// Folds the flattened v0 error fields into the nested `error` object.
fn legacy_error_reason(reason: &Map<String, Value>) -> Result<Value, MigrationError> {
    if let Some(failure) = reason.get("failure") {
        let failure = failure.as_object().ok_or_else(|| {
            invalid(
                FormatVersion::V0,
                "`turn/end.reason.failure` must be an object",
            )
        })?;
        for key in ["message", "code"] {
            if string_field(failure, key).is_none() {
                return Err(invalid(
                    FormatVersion::V0,
                    format!("`turn/end.reason.failure.{key}` must be a string"),
                ));
            }
        }
        return Ok(serde_json::json!({ "kind": "error", "error": failure }));
    }
    let message = string_field(reason, "message").ok_or_else(|| {
        invalid(
            FormatVersion::V0,
            "`turn/end.reason` must carry `failure` or `message`",
        )
    })?;
    let code = string_field(reason, "code").unwrap_or("UNKNOWN");
    Ok(serde_json::json!({ "kind": "error", "error": { "message": message, "code": code } }))
}

fn normalize_request_header(root: &mut Map<String, Value>) -> Result<(), MigrationError> {
    let data = data_object_mut(root)?;
    if data.get("reason").and_then(Value::as_str) == Some("fallback") {
        return Err(unsupported(
            FormatVersion::V0,
            FormatVersion::V1,
            "the retired `request/header` reason `fallback` has no current equivalent",
        ));
    }
    let Some(header) = data.get_mut("header").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    if !header.contains_key("messagePrefix") {
        return Ok(());
    }
    if !header.get("messagePrefix").is_some_and(Value::is_array) {
        return Err(invalid(
            FormatVersion::V0,
            "`request/header.header.messagePrefix` must be an array",
        ));
    }
    // The prefix duplicated the message list the surface already records; keeping it would show a
    // reader the same instruction twice.
    header.remove("messagePrefix");
    Ok(())
}

fn normalize_retry(
    root: &mut Map<String, Value>,
    session: Option<&str>,
) -> Result<(), MigrationError> {
    let seq = seq_of(root)?;
    let data = data_object_mut(root)?;
    if data.contains_key("retryId") {
        return Ok(());
    }
    data.insert(
        "retryId".to_owned(),
        Value::from(mint("legacy-retry", session, seq)?),
    );
    Ok(())
}

fn normalize_compaction_start(
    root: &mut Map<String, Value>,
    session: Option<&str>,
) -> Result<(), MigrationError> {
    let seq = seq_of(root)?;
    let data = data_object_mut(root)?;
    if data.contains_key("compactionId") {
        return Ok(());
    }
    data.insert(
        "compactionId".to_owned(),
        Value::from(mint("legacy-compaction", session, seq)?),
    );
    Ok(())
}

fn normalize_user_message(
    root: &mut Map<String, Value>,
    session: Option<&str>,
) -> Result<(), MigrationError> {
    let seq = seq_of(root)?;
    let data = data_object_mut(root)?;

    // `steering/message` carried `turn` beside the message; the turn is implied by position now.
    data.remove("turn");

    // A `steering/message` may already nest its message; unwrapping makes it identical to a v1
    // `user/message` payload, which is why the rename and the unwrap can share one code path.
    if let Some(wrapped) = data.remove("message") {
        let mut message = wrapped.as_object().cloned().ok_or_else(|| {
            invalid(
                FormatVersion::V0,
                "`steering/message.message` must be an object",
            )
        })?;
        if !message.contains_key("id") && !message.contains_key("role") {
            message.insert(
                "id".to_owned(),
                Value::from(mint("legacy-message", session, seq)?),
            );
            message.insert("role".to_owned(), Value::from("user"));
        }
        *data = message;
        return Ok(());
    }

    if is_legacy_message_shape(data) {
        data.insert(
            "id".to_owned(),
            Value::from(mint("legacy-message", session, seq)?),
        );
        data.insert("role".to_owned(), Value::from("user"));
    }
    normalize_compaction_attribution(data)
}

/// Adds the open compaction's id to a summary message's plugin source.
///
/// The id lives on the `compaction/start` row, so a message seen alone cannot be attributed.
fn normalize_compaction_attribution(data: &mut Map<String, Value>) -> Result<(), MigrationError> {
    let Some(source) = data.get("source").and_then(Value::as_object) else {
        return Ok(());
    };
    let is_compaction = source.get("kind").and_then(Value::as_str) == Some("plugin")
        && source.get("plugin").and_then(Value::as_str) == Some("compact");
    if !is_compaction || source.contains_key("compactionId") {
        return Ok(());
    }
    Err(unsupported(
        FormatVersion::V0,
        FormatVersion::V1,
        "this compaction summary cites no `compactionId`, so it needs the id from its opening \
         `compaction/start`; migrate the artifact in order rather than this row alone",
    ))
}

fn normalize_assistant_message(
    root: &mut Map<String, Value>,
    session: Option<&str>,
) -> Result<(), MigrationError> {
    let seq = seq_of(root)?;
    let data = data_object_mut(root)?;
    if data.contains_key("message") {
        return Ok(());
    }
    let (Some(content), Some(provenance)) = (data.get("content"), data.get("provenance")) else {
        return Ok(());
    };
    let mut source = provenance.as_object().cloned().ok_or_else(|| {
        invalid(
            FormatVersion::V0,
            "`assistant/message.provenance` must be an object",
        )
    })?;
    // v0 recorded who produced the answer beside the content; v1 nests it in the message source.
    source.insert("kind".to_owned(), Value::from("model"));
    let message = serde_json::json!({
        "id": mint("legacy-message", session, seq)?,
        "role": "assistant",
        "content": content,
        "source": source,
    });
    data.remove("content");
    data.remove("provenance");
    data.insert("message".to_owned(), message);
    Ok(())
}

fn normalize_tool_result(
    root: &mut Map<String, Value>,
    session: Option<&str>,
) -> Result<(), MigrationError> {
    let seq = seq_of(root)?;
    let data = data_object_mut(root)?;
    if data.contains_key("message") {
        return Ok(());
    }
    let (Some(call_id), Some(is_error), Some(content)) = (
        data.get("callId").and_then(Value::as_str),
        data.get("isError").and_then(Value::as_bool),
        data.get("content"),
    ) else {
        return Ok(());
    };
    if data
        .get("surfaceOp")
        .and_then(Value::as_object)
        .is_some_and(|op| op.get("op").and_then(Value::as_str) == Some("replace"))
    {
        return Err(unsupported(
            FormatVersion::V0,
            FormatVersion::V1,
            "a replacement `tool/result` inherits the identity of the message it replaces, which \
             was minted on an earlier row",
        ));
    }
    let message = serde_json::json!({
        "id": mint("legacy-message", session, seq)?,
        "role": "user",
        "content": [{
            "type": "tool-result",
            "toolCallId": call_id,
            "content": content,
            "isError": is_error,
        }],
        "source": { "kind": "tool", "callId": call_id },
    });
    data.remove("callId");
    data.remove("content");
    data.remove("isError");
    data.insert("message".to_owned(), message);
    Ok(())
}

/// Mints a deterministic legacy id.
///
/// Deterministic because the id appears in later rows' references: a random one would break every
/// id a previous migration run already wrote. The session id is part of it so two logs cannot
/// collide.
fn mint(prefix: &str, session: Option<&str>, seq: u64) -> Result<String, MigrationError> {
    let Some(session) = session else {
        return Err(unsupported(
            FormatVersion::V0,
            FormatVersion::V1,
            format!(
                "this row needs a `{prefix}` id derived from the session id, which a bare row does \
                 not carry; use `upgrade_in_session`"
            ),
        ));
    };
    Ok(format!("{prefix}:{session}:{seq}"))
}

// ---- v3 -> v4 ---------------------------------------------------------------------------------

/// Converts one released-v3 row into the current generation.
///
/// Two parts of the upstream edge are insertions and are therefore not performed here: closing an
/// interrupted turn with a synthetic `turn/end`, and appending missing `subagent/catalog` facts.
/// Both need rows the caller is not currently holding, and both only ever *add* events, so a log
/// converted row-by-row is otherwise a faithful v4 log.
fn migrate_v3_to_v4(root: &mut Map<String, Value>) -> Result<(), MigrationError> {
    let tag = string_field(root, "type")
        .ok_or_else(|| invalid(FormatVersion::V3, "row lacks a string `type`"))?
        .to_owned();

    if tag == "session" {
        return migrate_v3_header(root);
    }

    // An ignorable event this generation never defined stays opaque; only its name moves, so a
    // reader cannot mistake an extension for a current lifecycle event. Its payload is not
    // traversed: v3 never validated those positions and v4 does not interpret them.
    if root.get("ignorable").and_then(Value::as_bool) == Some(true)
        && !RELEASED_V3_EVENT_TYPES.contains(&tag.as_str())
    {
        root.insert("type".to_owned(), Value::from(format!("plugin:{tag}")));
        return Ok(());
    }
    if !RELEASED_V3_EVENT_TYPES.contains(&tag.as_str()) {
        return Err(unsupported(
            FormatVersion::V3,
            FormatVersion::V4,
            format!("v3 never defined the event type `{tag}`, so its meaning cannot be preserved"),
        ));
    }
    if root
        .get("data")
        .and_then(Value::as_object)
        .is_some_and(is_legacy_replacement)
    {
        return Err(MigrationError::VersionMismatch {
            claimed: FormatVersion::V3,
            detected: FormatVersion::V2,
        });
    }

    rewrite_message_sources(&tag, root)?;
    lift_tool_result(&tag, root)?;
    migrate_content(&tag, root)?;
    Ok(())
}

/// Advances the logical header and validates the fields v4 still requires.
fn migrate_v3_header(root: &mut Map<String, Value>) -> Result<(), MigrationError> {
    if root.get("version").and_then(Value::as_u64) != Some(3) {
        return Err(invalid(
            FormatVersion::V3,
            "header does not carry `version: 3`",
        ));
    }
    for key in ["id", "createdAt", "isSeeded", "delegationDepth"] {
        if !root.contains_key(key) {
            return Err(invalid(FormatVersion::V3, format!("header lacks `{key}`")));
        }
    }
    for key in ["parentSession", "agentPreset"] {
        if root.get(key).is_some_and(|value| !value.is_string()) {
            return Err(invalid(
                FormatVersion::V3,
                format!("header `{key}` must be a string"),
            ));
        }
    }
    if let Some(cwd) = root.get("cwd").and_then(Value::as_str) {
        if !std::path::Path::new(cwd).is_absolute() {
            return Err(invalid(FormatVersion::V3, "header `cwd` must be absolute"));
        }
    }
    if root
        .get("origin")
        .is_some_and(|origin| origin != "subagent")
    {
        return Err(invalid(
            FormatVersion::V3,
            "header `origin` must be `subagent`",
        ));
    }
    root.insert("version".to_owned(), Value::from(4));
    Ok(())
}

/// Applies `apply` to every message at a declared position.
fn for_each_message<F>(
    tag: &str,
    data: &mut Map<String, Value>,
    apply: &mut F,
) -> Result<(), MigrationError>
where
    F: FnMut(&mut Map<String, Value>) -> Result<(), MigrationError>,
{
    match tag {
        "user/message" => apply(data),
        "assistant/message" | "developer/message" | "system/message" | "tool/result" => {
            let message = data
                .get_mut("message")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    invalid(
                        FormatVersion::V3,
                        format!("`{tag}` requires an object `message`"),
                    )
                })?;
            apply(message)
        }
        "agent/inbox/spliced" => for_each_in_array(tag, data, "inserted", apply),
        "session/title-llm-request" => for_each_in_array(tag, data, "messages", apply),
        _ => Ok(()),
    }
}

fn for_each_in_array<F>(
    tag: &str,
    data: &mut Map<String, Value>,
    key: &str,
    apply: &mut F,
) -> Result<(), MigrationError>
where
    F: FnMut(&mut Map<String, Value>) -> Result<(), MigrationError>,
{
    let items = data
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            invalid(
                FormatVersion::V3,
                format!("`{tag}.{key}` must be an array of message objects"),
            )
        })?;
    for item in items {
        let message = item.as_object_mut().ok_or_else(|| {
            invalid(
                FormatVersion::V3,
                format!("`{tag}.{key}` holds a non-object message"),
            )
        })?;
        apply(message)?;
    }
    Ok(())
}

/// Converts every declared plugin source into its producer kind.
fn rewrite_message_sources(tag: &str, root: &mut Map<String, Value>) -> Result<(), MigrationError> {
    let seq = seq_of(root)?;
    let data = data_object_mut(root)?;
    for_each_message(tag, data, &mut |message| rewrite_source(message, seq))
}

fn rewrite_source(message: &mut Map<String, Value>, seq: u64) -> Result<(), MigrationError> {
    let Some(source) = message.get("source").and_then(Value::as_object) else {
        return Ok(());
    };
    let kind = string_field(source, "kind")
        .filter(|kind| !kind.is_empty())
        .ok_or_else(|| {
            invalid(
                FormatVersion::V3,
                format!("the message source at seq {seq} requires a nonempty `kind`"),
            )
        })?;
    if kind != "plugin" {
        // A direct source keeps its kind and every own field: v4 needs no translation for it.
        return Ok(());
    }
    let plugin = string_field(source, "plugin")
        .ok_or_else(|| {
            invalid(
                FormatVersion::V3,
                format!("the plugin source at seq {seq} requires a string `plugin`"),
            )
        })?
        .to_owned();
    let role = string_field(message, "role");
    let producer = producer_kind(&plugin, role);

    let mut replacement = source.clone();
    replacement.remove("plugin");
    replacement.insert("kind".to_owned(), Value::from(producer));
    message.insert("source".to_owned(), Value::Object(replacement));
    Ok(())
}

/// Resolves the current producer kind for one released plugin name.
///
/// An unknown plugin keeps its complete original name behind a `plugin:` prefix, so two distinct
/// extension producers never collapse into one kind.
fn producer_kind(plugin: &str, role: Option<&str>) -> String {
    if plugin == "@deepseek-ai/dsh-system-prompt" {
        return if role == Some("system") {
            "system-prompt".to_owned()
        } else {
            "runtime-context".to_owned()
        };
    }
    let renamed = match plugin {
        "compact" => Some("compact-checkpoint"),
        "tools-code-mode" | "tools-ptc" => Some("ptc-mode"),
        "dsh-compaction-basic" => Some("compact-basic"),
        _ => None,
    };
    if let Some(kind) = renamed {
        return kind.to_owned();
    }
    if SAME_NAME_PRODUCERS.contains(&plugin) {
        return plugin.to_owned();
    }
    format!("plugin:{plugin}")
}

/// Lifts a wrapper `tool/result` into a first-class `tool`-role message.
///
/// The wrapper is validated before it is dropped: a malformed historical result must be refused,
/// not silently truncated into a well-formed but wrong message.
fn lift_tool_result(tag: &str, root: &mut Map<String, Value>) -> Result<(), MigrationError> {
    if tag != "tool/result" {
        return Ok(());
    }
    let seq = seq_of(root)?;
    let data = data_object_mut(root)?;
    let Some(message) = data.get("message").and_then(Value::as_object) else {
        return Ok(());
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        // Already lifted, or a representation this converter does not claim.
        return Ok(());
    }
    let source = message.get("source").and_then(Value::as_object);
    let call_id = source.and_then(|source| string_field(source, "callId"));
    let wrapper = message
        .get("content")
        .and_then(Value::as_array)
        .filter(|blocks| blocks.len() == 1)
        .and_then(|blocks| blocks.first())
        .and_then(Value::as_object);
    let id = string_field(message, "id").filter(|id| !id.is_empty());
    let (Some(source), Some(call_id), Some(wrapper), Some(id)) = (source, call_id, wrapper, id)
    else {
        return Err(invalid(
            FormatVersion::V3,
            format!(
                "`tool/result` at seq {seq} requires a message id, a tool source with a call id, \
                 and exactly one content block"
            ),
        ));
    };
    if source.get("kind").and_then(Value::as_str) != Some("tool")
        || call_id.is_empty()
        || wrapper.get("type").and_then(Value::as_str) != Some("tool-result")
        || wrapper.get("toolCallId").and_then(Value::as_str) != Some(call_id)
    {
        return Err(invalid(
            FormatVersion::V3,
            format!(
                "`tool/result` at seq {seq} requires exactly one `tool-result` wrapper matching \
                 its tool source"
            ),
        ));
    }
    let is_error = match wrapper.get("isError") {
        None => None,
        Some(value) => Some(value.as_bool().ok_or_else(|| {
            invalid(
                FormatVersion::V3,
                format!("`tool/result` at seq {seq} has a non-boolean `isError`"),
            )
        })?),
    };
    let content = wrapper
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            invalid(
                FormatVersion::V3,
                format!("`tool/result` at seq {seq} requires array wrapper content"),
            )
        })?;
    if content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool-result"))
    {
        return Err(unsupported(
            FormatVersion::V3,
            FormatVersion::V4,
            format!(
                "`tool/result` at seq {seq} nests another tool result, which this converter does \
                 not lift"
            ),
        ));
    }

    let mut lifted = Map::new();
    lifted.insert("role".to_owned(), Value::from("tool"));
    lifted.insert("source".to_owned(), Value::Object(source.clone()));
    lifted.insert("toolCallId".to_owned(), Value::from(call_id));
    lifted.insert("content".to_owned(), Value::Array(content.clone()));
    if let Some(is_error) = is_error {
        lifted.insert("isError".to_owned(), Value::Bool(is_error));
    }
    lifted.insert("id".to_owned(), Value::from(id));
    // Fields on either owner are preserved under an owner-qualified name: merging them into one
    // namespace would let a wrapper key overwrite a message key.
    for (key, value) in message {
        if !matches!(key.as_str(), "id" | "role" | "source" | "content") {
            lifted.insert(format!("plugin:message:{key}"), value.clone());
        }
    }
    for (key, value) in wrapper {
        if !matches!(key.as_str(), "type" | "toolCallId" | "content" | "isError") {
            lifted.insert(format!("plugin:result:{key}"), value.clone());
        }
    }
    data.insert("message".to_owned(), Value::Object(lifted));
    Ok(())
}

/// Namespaces extension content tags at every position v4 interprets.
fn migrate_content(tag: &str, root: &mut Map<String, Value>) -> Result<(), MigrationError> {
    let seq = seq_of(root)?;
    let data = data_object_mut(root)?;
    let subject = format!("format v3 `{tag}` at seq {seq}");

    for_each_message(tag, data, &mut |message| {
        if let Some(content) = message.get_mut("content") {
            namespace_blocks(content, &subject)?;
        }
        Ok(())
    })?;

    match tag {
        "compaction/summary" => {
            if let Some(summary) = data.get_mut("summary") {
                namespace_blocks(summary, &subject)?;
            }
            if let Some(raw) = data.get_mut("rawOutput") {
                namespace_blocks(raw, &subject)?;
            }
            Ok(())
        }
        "tool/ptc-dispatch" => {
            if let Some(content) = data.get_mut("content") {
                namespace_blocks(content, &subject)?;
            }
            Ok(())
        }
        "team/message/queued" => {
            let Some(message) = data.get_mut("message").and_then(Value::as_object_mut) else {
                return Ok(());
            };
            if let Some(content) = message.get_mut("content") {
                namespace_blocks(content, &subject)?;
            }
            Ok(())
        }
        "request/header" => refuse_deferred_loading(data, &subject),
        "assistant/message" | "assistant/attempt" => namespace_stream(data, &subject),
        _ => Ok(()),
    }
}

/// Prefixes every content tag v3 left open with `plugin:`.
fn namespace_blocks(value: &mut Value, subject: &str) -> Result<(), MigrationError> {
    let blocks = value.as_array_mut().ok_or_else(|| {
        invalid(
            FormatVersion::V3,
            format!("{subject} requires an array of content blocks"),
        )
    })?;
    for (index, block) in blocks.iter_mut().enumerate() {
        namespace_block(block, &format!("{subject} content block {index}"))?;
    }
    Ok(())
}

/// Prefixes one content block's tag when v3 did not define it.
fn namespace_block(block: &mut Value, subject: &str) -> Result<(), MigrationError> {
    let object = block
        .as_object_mut()
        .ok_or_else(|| invalid(FormatVersion::V3, format!("{subject} is not an object")))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(FormatVersion::V3, format!("{subject} has no string `type`")))?
        .to_owned();
    if !CONTENT_KINDS.contains(&kind.as_str()) {
        object.insert("type".to_owned(), Value::from(format!("plugin:{kind}")));
    }
    Ok(())
}

/// Namespaces the block tags inside an embedded assistant stream.
///
/// The stream carries the same block vocabulary in two shapes: a completed `block` on `block-end`
/// and a bare `blockType` on `block-start`. Both must move together or a reader would see a start
/// and an end describing different kinds.
fn namespace_stream(data: &mut Map<String, Value>, subject: &str) -> Result<(), MigrationError> {
    let Some(stream) = data.get_mut("stream").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for (index, entry) in stream.iter_mut().enumerate() {
        let Some(entry) = entry.as_object_mut() else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("chunk") {
            continue;
        }
        let Some(chunk) = entry.get_mut("chunk").and_then(Value::as_object_mut) else {
            continue;
        };
        match chunk.get("type").and_then(Value::as_str) {
            Some("block-end") => {
                if let Some(block) = chunk.get_mut("block") {
                    namespace_block(block, &format!("{subject} stream entry {index} block"))?;
                }
            }
            Some("block-start") => {
                let original = chunk
                    .get("blockType")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        invalid(
                            FormatVersion::V3,
                            format!("{subject} stream entry {index} has no string `blockType`"),
                        )
                    })?
                    .to_owned();
                if !CONTENT_KINDS.contains(&original.as_str()) {
                    chunk.insert(
                        "blockType".to_owned(),
                        Value::from(format!("plugin:{original}")),
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Refuses a request-tool definition that already carries the v4-only `deferLoading` field.
///
/// The field did not exist in v3, so a v3 row holding one has a meaning this edge cannot assign;
/// guessing would silently change when a tool is loaded.
fn refuse_deferred_loading(
    data: &mut Map<String, Value>,
    subject: &str,
) -> Result<(), MigrationError> {
    let tools = data
        .get("header")
        .and_then(Value::as_object)
        .and_then(|header| header.get("tools"))
        .and_then(Value::as_array);
    let Some(tools) = tools else {
        return Ok(());
    };
    if let Some(index) = tools
        .iter()
        .position(|tool| tool.get("deferLoading").is_some())
    {
        return Err(unsupported(
            FormatVersion::V3,
            FormatVersion::V4,
            format!(
                "{subject}.header.tools[{index}] carries `deferLoading`, which only v4 defines"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_advance_one_generation_at_a_time() {
        assert_eq!(FormatVersion::V0.next(), Some(FormatVersion::V1));
        assert_eq!(FormatVersion::V3.next(), Some(FormatVersion::V4));
        assert_eq!(FormatVersion::V4.next(), None);
        assert!(FormatVersion::V4.is_current());
        assert_eq!(FormatVersion::V3.as_number(), 3);
        assert_eq!(FormatVersion::from_number(9), None);
    }

    #[test]
    fn the_chain_describes_all_four_edges() {
        let edges = Migration::edges();
        assert_eq!(edges.len(), 4);
        assert_eq!(edges.iter().filter(|edge| edge.is_supported()).count(), 2);
        assert!(
            Migration::new(FormatVersion::V1, FormatVersion::V2)
                .unsupported_reason()
                .is_some()
        );
    }
}
