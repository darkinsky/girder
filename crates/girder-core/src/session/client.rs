//! The client *stream transport* of upstream `packages/api/session-controller`: the two adapters that
//! bind Gateway-owned Remote stream lifecycles to Session state, and the wire-envelope acceptance
//! gate every history page and follow frame passes through.
//!
//! # Upstream capability distribution, measured
//!
//! `packages/api/session-controller/src` is 41 sources and 9,413 lines. Splitting them by directory
//! answers the question "where does this package's work actually live":
//!
//! | Area | Sources | Lines | What it owns |
//! | --- | ---: | ---: | --- |
//! | `client/sessions/` | 9 | 3,398 | The object layer: `session.ts` (1,055), `manager.ts` (857), `service.ts` (741), `projection-store.ts` (244), `assistant-stream.ts` (229), `notifier.ts` (97), `lineage.ts` (96), `remotes.ts` (44), `history-records.ts` (35) |
//! | `client/` (root) | 6 | 575 | **The stream transport**: `transport.ts` (247), `index.ts` (149), `scope.ts` (76), `session-wire-event.ts` (46), `ordered-baseline.ts` (43), `time-zone.ts` (14) |
//! | `client/contract/` | 4 | 642 | The outward faces and entry vocabularies: `events.ts` (205), `session.ts` (174), `sessions.ts` (169), `snapshot.ts` (94) |
//! | top level | 15 | 3,723 | `types.ts` (720), `index.ts` (651), `commands.ts` (602), `agent.ts` (474), `list.ts` (382), `catalog.ts` (117), `continuation.ts` (58), and the Host controllers |
//! | `history/` | 3 | 660 | `listener.ts` (302), `controller.ts` (256), `assistant-stream.ts` (102) |
//! | `state/` | 2 | 337 | `controller.ts` (221), `listener.ts` (116) |
//! | `web/` | 1 | 78 | `web/controller.ts` |
//!
//! The three-way split inside `client/` is therefore **contract 642 / transport 575 / object layer
//! 3,398**. The object layer is where the bulk sits, which is the useful negative result: the
//! package's centre of gravity is long-lived objects, not the stream.
//!
//! # What the sibling session modules already cover
//!
//! Each was read for this report rather than assumed from its name:
//!
//! - [`girder_core::session::controller`] (683 lines) — `PAGE_MESSAGES`, `JUMP_PAGE_MESSAGES`,
//!   `DEFAULT_MAX_MESSAGES`, `paginate`, `page_records`, the tri-state `SessionCursor`, the
//!   message-aligned cut arithmetic, address resolution, `merge_ordered_baseline`, `flatten_lineage`,
//!   `history_record_first_seq`. This covers `history.ts` and the client's pure ordering helpers.
//! - [`girder_core::session::engine`] (789 lines) — `ProjectionValueStore`, `Notifier`,
//!   `SeqCursor`: the `projection-store.ts` and `notifier.ts` halves of `client/sessions/`.
//! - [`girder_core::session::snapshot`] (2,426 lines) — `SessionView`, `ListEngine`,
//!   `SessionSnapshot`, `PendingSubmission`, `OpenState`, `PromptError`, `ControlFrame`,
//!   `ControlBaseline`, `ProjectionUpdate`, the reconnect plan.
//! - [`girder_core::session::window`] (1,534 lines) — `EventWindow`/`MutableSessionEventSource`,
//!   `SessionEntry`, `EventWindowChange`, `ClientAssistantStream`, the settlement fold: the whole of
//!   `client/contract/events.ts` plus `client/sessions/assistant-stream.ts`.
//! - [`girder_core::session::extra`] (2,133 lines) — storage generations, headers, framing,
//!   `KNOWN_EVENT_TYPES`, `validate_stored_records`.
//!
//! Those five leave exactly one whole upstream file unclaimed: **`client/transport.ts`**, together
//! with its `client/session-wire-event.ts` dependency and the three-line `history-records.ts` helper
//! it leans on. Grepped before this module existed, `createSessionControlStream`,
//! `SessionEventStream`, `SessionJournalChange`, `ClientSessionPageRequest`, `SessionControlStream`,
//! `SessionControlStreamOptions`, `SessionEventStreamOptions`, `SessionRemote`, `SessionRemotes`,
//! `resolvedClientTimeZone`, `assertSessionWireEvent`, `historyEntries` and `historyRecordLastSeq` all
//! had **zero** hits in this crate.
//!
//! # What is still completely uncovered after this module
//!
//! Recorded so the next porter does not have to re-derive it. Grepped, not guessed:
//!
//! - **`client/sessions/session.ts` (1,055)** — the `Session` object: prompt/stop/interrupt, queue
//!   and steering placement, attachment upload, fork-at-seq, the volatile-to-durable replacement.
//!   (`session_snapshot_gap` models the *snapshot* this object publishes, and
//!   `begin_submission`/`retire_submission` for its echo list; it does not model the object.)
//! - **`client/sessions/service.ts` (741)** — `ClientSessions`, `SessionBinding`,
//!   `SessionCreateError`, `SessionForkError`, and the `retain`/`using`/`release` reference counting
//!   with per-source counts. There are **zero** occurrences of a retain/refcount API in this crate.
//! - **`client/sessions/manager.ts` (857)** — `SessionManager`, the list/search phase machine, the
//!   projection-snapshot reconciliation. (`session_snapshot_gap`'s `ListEngine` is the pull protocol
//!   `manager.ts` drives, not the manager's phase machine.)
//! - **`client/sessions/remotes.ts` (44)** — `SessionRemotes`, the typed facade over the generated
//!   `remote.session`, `remote.commands` and `remote.subagents` namespaces.
//! - **`client/scope.ts` (76)** — `createScope`/`scopeOf`/`scopeIdentityOf`: Cordis fibers carrying an
//!   agent tag plus a dispatch filter that admits untagged listeners globally and tagged ones only
//!   for the same generation. `agent_scope_gap` has an unrelated `createScope` (agent presets) and
//!   `scoped_events_gap` an unrelated `scopeOf`; neither carries the per-generation identity.
//! - **`client/time-zone.ts` (14)** — `resolvedClientTimeZone`, a browser `Intl` probe with no domain
//!   content. Deliberately not ported: this crate has no browser, and inventing a fake report of a
//!   browser's zone would be worse than the absence.
//! - **`client/contract/*` (642)** — `ISessions`, `ISession`, `SessionFace`, `ProjectionsFace`,
//!   `SubmissionHandle`, `SessionRetainInfo`, `SessionAssertInfo`, `SessionTarget`. The *vocabulary*
//!   of the object layer is as absent as the objects, which is the honest read: it is not that the
//!   names are missing from a header file, it is that nothing implements them.
//!
//! # What this module ports
//!
//! Everything in `client/transport.ts` and `client/session-wire-event.ts` that does not need a live
//! socket, expressed as decisions a caller feeds frames into:
//!
//! 1. **Wire-envelope acceptance** — [`WireEvent::accept`]. Seven admitted fields and no others; a
//!    non-negative safe-integer `seq` that is not `-0`; a safe-integer `time`; a present, non-null
//!    `data`; `ignorable` only ever `true`. Upstream's `assertSessionWireEvent` is an assertion
//!    function, so acceptance has no value there; here the acceptance *is* the value, which makes
//!    "validated" a property of the type rather than a comment on a `unknown`.
//! 2. **Event-local surface metadata** — [`WireEvent::accept`] also enforces the three rules
//!    upstream delegates to `validateSurfaceMetadata`: a surface-eligible event must carry a marker;
//!    a replacement must cite only earlier sequences and have the exact three-key shape; sources must
//!    be a non-empty, duplicate-free array of earlier sequences, and `assistant/message` — which
//!    embeds its own stream — may carry none at all.
//! 3. **Event-local payload rules** — the part of `validateSessionEventData` that is a property of one
//!    event rather than of the log: `developer/message` and the developer role occur together, a
//!    tool-change block requires that role, `tool-addition` requires a non-empty `toolName` and no
//!    inline definition, `headerSeq` appears exactly when an addition does, `request/header` carries
//!    no `system`, no empty `tools`, and no empty `adapterDefaults`, and `tool/result` with an `error`
//!    must mark its message `isError`.
//! 4. **Assistant-stream revision continuity** — [`AssistantRevision`]. The revision is a *dense*
//!    counter over the process-local Assistant lifecycle; a skip means the carrier dropped a frame.
//!    The durable cursor must not move, because nothing durable was lost, and the only honest
//!    recovery is a rebaseline. This is upstream's `session assistant stream skipped revision N`
//!    branch — the one stream failure that is deliberately *recoverable* rather than terminal.
//! 5. **The opted-in opening baseline rule** — [`JournalStream`] refuses an opening that omitted the
//!    Assistant baseline it asked for, and refuses an Assistant frame that arrives before any
//!    baseline. An absent baseline is not "no active attempt": it means the peer ignored the opt-in,
//!    and assuming an empty baseline would silently drop a live answer.
//! 6. **The journal change mapping** — [`FollowChange`], including the rename of Gateway's
//!    `notification` to Session's `assistant-stream`, the only structural difference between the two
//!    change unions.
//! 7. **Page/follow request scoping** — [`ClientPageRequest`] carries only the fields bound to an
//!    already-addressed journal, and [`ClientPageRequest::repair_request`] rebuilds the unbounded
//!    tail for reconnect repair.
//! 8. **The control-stream snapshot protocol** — [`ControlStream`], plus [`EndKind`], which is
//!    upstream's `ended: accepted => accepted ? carrier : protocol` classification.
//!
//! # Rules that are load-bearing, and why
//!
//! - **A revision gap is recoverable; a missing baseline is not.** The first says "we lost some
//!   transient frames, re-open". The second says "the peer did not honour the request", and no amount
//!   of retrying changes that. Treating them alike either loops forever on the second or gives up on
//!   the first.
//! - **A second opening in one generation is a violation.** Two `opened` frames mean the peer
//!   restarted the lifecycle without a new generation id, so every later revision comparison is
//!   against the wrong base.
//! - **An entry before the opening cursor is a violation, not a prepend.** The window has no
//!   established cut yet, so the entry cannot be placed relative to anything.
//! - **`-0` is not `0` on the wire.** JavaScript's `Object.is(-0, 0)` is `false`, and upstream relies
//!   on it. `serde_json` with `arbitrary_precision` keeps `-0` as the token `-0` while `as_i64`
//!   reports `Some(0)`, so a numeric check would silently accept it. The check here reads the token.
//! - **This gate is stricter than the storage gate, on purpose.** `validate_stored_records` accepts an
//!   unknown event type when the writer marked it ignorable, because a log may come from a newer
//!   harness. The wire gate instead follows upstream and lets *unknown types* through (they are
//!   merge-extensible) while refusing an unknown *field* on a current-format envelope: an extra field
//!   means the peer speaks a different protocol version, and dropping it would discard part of the
//!   event.
//!
//! # Module-include note
//!
//! This file is compiled by path from `tests/session_client_gap.rs`, so every reference to the
//! library goes through `girder_core::…` rather than `crate::…`: inside a `#[path]` module, `crate::`
//! names the *test* crate.

use std::fmt;

use serde_json::Value;

use girder_core::session::controller::{HistoryError, TurnWindow};
use girder_core::session::snapshot::{ControlBaseline, ControlFrame};
use girder_core::{MAX_SAFE_INTEGER, SurfaceOp, is_surface_eligible_type};

/// The event types this build can decode.
///
/// Mirrors [`KNOWN_EVENT_TYPES`](girder_core::KNOWN_EVENT_TYPES), which is not reachable from here as
/// a value: `session_extra_gap` is a private module and only some of its items are re-exported. The
/// distinction this module draws — unknown *type* on an ignorable row keeps opaque metadata, known
/// non-surface type may not carry it — needs only membership, so the list is restated and pinned
/// against the real vocabulary by the test suite. It is the same 18 types.
const KNOWN_TYPES: [&str; 18] = [
    "turn/start",
    "turn/end",
    "step/start",
    "step/end",
    "user/message",
    "developer/message",
    "assistant/message",
    "model/usage",
    "tool/call",
    "tool/result",
    "system/message",
    "model/selection",
    "permission/preset",
    "plan/mode",
    "sandbox/mode",
    "request/context",
    "session/title",
    "llm/retry",
];

/// Whether this build can decode an event type.
fn is_known_event_type(name: &str) -> bool {
    KNOWN_TYPES.contains(&name)
}

/// The envelope fields upstream admits, and no others.
const ENVELOPE_FIELDS: [&str; 7] = [
    "type",
    "seq",
    "time",
    "data",
    "ignorable",
    "surfaceOp",
    "sourceEventSeqs",
];

// ---------------------------------------------------------------------------------------------
// Wire values
// ---------------------------------------------------------------------------------------------

/// Why a wire event or page was refused.
///
/// The variants are upstream's `assertSessionWireEvent` throws, in its order: envelope shape, then
/// event-local surface metadata, then event-local payload rules.
#[derive(Clone, Debug, PartialEq)]
pub enum WireError {
    /// The value is not a JSON object.
    NotAnObject,
    /// The object carries a field this build does not understand.
    ///
    /// Upstream refuses instead of ignoring because a *current-format* envelope with an extra field
    /// was written by a peer speaking a newer protocol.
    UnexpectedField {
        /// The offending field name.
        field: String,
    },
    /// `type` is missing or is not a string.
    MissingType,
    /// `seq` is missing, not a safe integer, negative, or `-0`.
    InvalidSeq,
    /// `time` is missing or is not a safe integer.
    InvalidTime,
    /// `data` is missing or is `null`.
    MissingData,
    /// `ignorable` is present but is not exactly `true`.
    InvalidIgnorable,
    /// A surface-eligible event carries no `surfaceOp` marker.
    MissingSurfaceOp {
        /// The event type.
        type_name: String,
    },
    /// `surfaceOp` is present on a non-surface event, or is malformed.
    InvalidSurfaceOp {
        /// What is wrong.
        detail: String,
    },
    /// `sourceEventSeqs` is present on an event that may not carry provenance.
    UnexpectedSources {
        /// The event type.
        type_name: String,
    },
    /// `sourceEventSeqs` is present but is not a usable array of earlier sequences.
    InvalidSources {
        /// What is wrong.
        detail: String,
    },
    /// An event-local payload rule failed.
    InvalidPayload {
        /// What is wrong.
        detail: String,
    },
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject => write!(formatter, "session wire event must be an object"),
            Self::UnexpectedField { field } => {
                write!(formatter, "session wire event has unexpected field {field}")
            }
            Self::MissingType => write!(formatter, "session wire event has no \"type\" string"),
            Self::InvalidSeq => write!(
                formatter,
                "session wire event has an invalid seq; it must be a non-negative safe integer \
                 and not -0"
            ),
            Self::InvalidTime => write!(formatter, "session wire event has an invalid time"),
            Self::MissingData => {
                write!(
                    formatter,
                    "session wire event has an invalid envelope (no data)"
                )
            }
            Self::InvalidIgnorable => write!(
                formatter,
                "session wire event has an invalid envelope (ignorable must be true when present)"
            ),
            Self::MissingSurfaceOp { type_name } => write!(
                formatter,
                "session event \"{type_name}\" is surface-eligible and requires a surfaceOp marker"
            ),
            Self::InvalidSurfaceOp { detail } => {
                write!(
                    formatter,
                    "session wire event has an invalid surfaceOp: {detail}"
                )
            }
            Self::UnexpectedSources { type_name } => write!(
                formatter,
                "session event \"{type_name}\" is not surface-eligible and cannot carry \
                 sourceEventSeqs"
            ),
            Self::InvalidSources { detail } => {
                write!(
                    formatter,
                    "session wire event has invalid sourceEventSeqs: {detail}"
                )
            }
            Self::InvalidPayload { detail } => {
                write!(formatter, "session wire event payload is invalid: {detail}")
            }
        }
    }
}

impl std::error::Error for WireError {}

/// One accepted wire event envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct WireEvent {
    /// The event type tag, exactly as received.
    pub type_name: String,
    /// Durable position.
    pub seq: u64,
    /// Millisecond timestamp, already range-checked.
    pub time: i64,
    /// The payload, untouched.
    pub data: Value,
    /// Whether a reader may skip this row.
    pub ignorable: bool,
    /// The validated placement, when the event is surface-eligible.
    ///
    /// Decoded rather than kept opaque: upstream's assertion runs the replacement-ordering checks,
    /// which need the ordered pair, and keeping the raw JSON alongside a decoded copy would leave two
    /// representations to disagree.
    pub surface_op: Option<SurfaceOp>,
    /// The raw `sourceEventSeqs`, kept opaque.
    ///
    /// Opaque because on an unknown-but-ignorable row upstream explicitly permits opaque metadata, and
    /// a typed `Vec<u64>` could not hold it.
    pub source_event_seqs: Option<Value>,
}

impl WireEvent {
    /// Accepts one wire event, or says precisely why it is not one.
    ///
    /// # Errors
    ///
    /// [`WireError`] for every envelope, surface-metadata, and event-local payload rule upstream's
    /// `assertSessionWireEvent` throws on.
    pub fn accept(value: &Value) -> Result<Self, WireError> {
        let object = value.as_object().ok_or(WireError::NotAnObject)?;
        for field in object.keys() {
            if !ENVELOPE_FIELDS.contains(&field.as_str()) {
                return Err(WireError::UnexpectedField {
                    field: field.clone(),
                });
            }
        }

        let type_name = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or(WireError::MissingType)?
            .to_owned();
        let seq = safe_unsigned(object.get("seq")).ok_or(WireError::InvalidSeq)?;
        let time = safe_signed(object.get("time")).ok_or(WireError::InvalidTime)?;
        let data = match object.get("data") {
            Some(value) if !value.is_null() => value.clone(),
            _ => return Err(WireError::MissingData),
        };
        let ignorable = match object.get("ignorable") {
            None => false,
            Some(Value::Bool(true)) => true,
            Some(_) => return Err(WireError::InvalidIgnorable),
        };
        let raw_op = object.get("surfaceOp");
        let source_event_seqs = object.get("sourceEventSeqs").cloned();

        let eligible = is_surface_eligible_type(&type_name);
        let surface_op = if eligible {
            let raw = raw_op.ok_or_else(|| WireError::MissingSurfaceOp {
                type_name: type_name.clone(),
            })?;
            Some(decode_surface_op(raw, seq)?)
        } else if !is_known_event_type(&type_name) && ignorable {
            // An unknown-but-ignorable row retains opaque metadata without touching the surface. The
            // fields are deliberately *not* inspected: this build cannot decode the type, so it cannot
            // rule on what its placement metadata means, and the row never changes the surface anyway.
            // A *known* non-surface type gets no such latitude — `ignorable` excuses an undecodable
            // event, not a misplaced marker on one this build understands.
            None
        } else {
            if raw_op.is_some() {
                return Err(WireError::InvalidSurfaceOp {
                    detail: format!("\"{type_name}\" is not surface-eligible"),
                });
            }
            if source_event_seqs.is_some() {
                return Err(WireError::UnexpectedSources {
                    type_name: type_name.clone(),
                });
            }
            None
        };

        // Opaque metadata on an unknown ignorable row is never re-interpreted, so the source rules
        // only apply to rows whose placement this build actually understands.
        if surface_op.is_some() || is_known_event_type(&type_name) {
            validate_sources(&type_name, seq, source_event_seqs.as_ref())?;
        }
        validate_payload(&type_name, &data)?;

        Ok(Self {
            type_name,
            seq,
            time,
            data,
            ignorable,
            surface_op,
            source_event_seqs,
        })
    }
}

/// Accepts one wire event envelope.
///
/// The free function exists because upstream has one, and because a caller holding a `Value` should
/// not have to know the type name just to validate it.
///
/// # Errors
///
/// Every [`WireError`] [`WireEvent::accept`] returns.
pub fn assert_session_wire_event(value: &Value) -> Result<WireEvent, WireError> {
    WireEvent::accept(value)
}

/// One accepted history record.
///
/// Upstream's `SessionHistoryRecord` is `{type:'event', event}` with a literal discriminator. The
/// literal is enforced here, so a record this build produces can only be one the transport can read
/// back.
#[derive(Clone, Debug, PartialEq)]
pub struct WireRecord {
    /// The accepted event.
    pub event: WireEvent,
}

impl WireRecord {
    /// Accepts one wire record.
    ///
    /// # Errors
    ///
    /// [`WireError::MissingType`] when the wrapper's `type` is not the literal `"event"`,
    /// [`WireError::UnexpectedField`] for any other wrapper field, and every [`WireError`]
    /// [`WireEvent::accept`] returns for the inner envelope.
    pub fn accept(value: &Value) -> Result<Self, WireError> {
        let object = value.as_object().ok_or(WireError::NotAnObject)?;
        if object.get("type").and_then(Value::as_str) != Some("event") {
            return Err(WireError::MissingType);
        }
        for field in object.keys() {
            if field != "type" && field != "event" {
                return Err(WireError::UnexpectedField {
                    field: field.clone(),
                });
            }
        }
        let event = object.get("event").ok_or(WireError::MissingData)?;
        Ok(Self {
            event: WireEvent::accept(event)?,
        })
    }

    /// Wraps an accepted event.
    #[must_use]
    pub fn event(event: WireEvent) -> Self {
        Self { event }
    }

    /// The first logical sequence this record covers.
    #[must_use]
    pub fn first_seq(&self) -> u64 {
        self.event.seq
    }

    /// The final logical sequence this record covers.
    ///
    /// Equal to [`WireRecord::first_seq`] today: a record stands at exactly one log position, and
    /// upstream keeps both functions because a future batch row would not.
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.event.seq
    }
}

/// Decodes a `surfaceOp`, enforcing upstream's exact replacement shape and ordering.
fn decode_surface_op(raw: &Value, seq: u64) -> Result<SurfaceOp, WireError> {
    if raw.as_str() == Some("append") {
        return Ok(SurfaceOp::Append);
    }
    let object = raw.as_object().ok_or_else(|| WireError::InvalidSurfaceOp {
        detail: "expected \"append\" or an object".to_owned(),
    })?;
    // Upstream's `isReplaceOp` requires *exactly* three keys, so an object with an extra field is a
    // different operation this build would otherwise misread as a plain replacement.
    if object.len() != 3 || !object.contains_key("op") {
        return Err(WireError::InvalidSurfaceOp {
            detail: "a replace marker must have exactly op, startSeq and endSeq".to_owned(),
        });
    }
    if object.get("op").and_then(Value::as_str) != Some("replace") {
        return Err(WireError::InvalidSurfaceOp {
            detail: "expected op \"replace\"".to_owned(),
        });
    }
    let start =
        safe_unsigned(object.get("startSeq")).ok_or_else(|| WireError::InvalidSurfaceOp {
            detail: "startSeq must be a non-negative safe integer".to_owned(),
        })?;
    let end = safe_unsigned(object.get("endSeq")).ok_or_else(|| WireError::InvalidSurfaceOp {
        detail: "endSeq must be a non-negative safe integer".to_owned(),
    })?;
    // A replacement must shadow *earlier* surface nodes. A range reaching this event's own seq would
    // let an event shadow itself, which makes the fold non-terminating.
    if start >= seq || end >= seq {
        return Err(WireError::InvalidSurfaceOp {
            detail: format!(
                "surface replace at seq {seq}: startSeq and endSeq must reference earlier events"
            ),
        });
    }
    Ok(SurfaceOp::Replace { start, end })
}

/// Enforces upstream's `assertSourceEventReferences` for an event-local view.
fn validate_sources(type_name: &str, seq: u64, raw: Option<&Value>) -> Result<(), WireError> {
    // An Assistant message embeds the stream it was assembled from, so citing extra sources would be a
    // second, contradicting provenance record.
    if type_name == "assistant/message" && raw.is_some() {
        return Err(WireError::InvalidSources {
            detail: "assistant/message embeds its source stream and cannot carry sourceEventSeqs"
                .to_owned(),
        });
    }
    let Some(raw) = raw else {
        return Ok(());
    };
    let array = raw.as_array().ok_or_else(|| WireError::InvalidSources {
        detail: format!("sourceEventSeqs on event at seq {seq} must be an array when present"),
    })?;
    if array.is_empty() {
        return Err(WireError::InvalidSources {
            detail: "sourceEventSeqs must not be empty".to_owned(),
        });
    }
    let mut seen = Vec::with_capacity(array.len());
    for source in array {
        let value = safe_unsigned(Some(source)).ok_or_else(|| WireError::InvalidSources {
            detail: format!(
                "session event \"{type_name}\" sourceEventSeqs must densely contain non-negative \
                 safe integers"
            ),
        })?;
        if seen.contains(&value) {
            return Err(WireError::InvalidSources {
                detail: "sourceEventSeqs must not contain duplicates".to_owned(),
            });
        }
        // A source that is not strictly earlier cannot be a provenance record: it either points at
        // this event or at one that did not exist when this event was written.
        if value >= seq {
            return Err(WireError::InvalidSources {
                detail: format!(
                    "sourceEventSeqs must reference earlier events: {value} >= current seq {seq}"
                ),
            });
        }
        seen.push(value);
    }
    Ok(())
}

/// Enforces the event-local half of upstream's `validateSessionEventData`.
///
/// Only rules that are decidable from one event are applied. `headerSeq` resolution against an actual
/// `request/header` needs the log, and stays Host-side; the *shape* rules that make such resolution
/// meaningful are here.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per wire event type, each a flat sequence of field checks; extracting them would spread one table of rules across four functions and make a reader diffing against the upstream validator jump between them"
)]
fn validate_payload(type_name: &str, data: &Value) -> Result<(), WireError> {
    match type_name {
        "developer/message" => {
            let message = data.get("message").filter(|value| value.is_object());
            let Some(message) = message else {
                return Ok(());
            };
            if message.get("role").and_then(Value::as_str) != Some("developer") {
                return Err(WireError::InvalidPayload {
                    detail: "developer/message requires the developer role".to_owned(),
                });
            }
            let blocks = message.get("content").and_then(Value::as_array);
            let mut has_additions = false;
            if let Some(blocks) = blocks {
                for block in blocks {
                    let kind = block.get("type").and_then(Value::as_str);
                    if kind != Some("tool-addition") && kind != Some("tool-removal") {
                        continue;
                    }
                    let named = block
                        .get("toolName")
                        .and_then(Value::as_str)
                        .is_some_and(|name| !name.is_empty());
                    if !named {
                        return Err(WireError::InvalidPayload {
                            detail: format!("{type_name} tool change requires a nonempty toolName"),
                        });
                    }
                    if kind == Some("tool-addition") {
                        has_additions = true;
                        if block.get("tool").is_some() {
                            return Err(WireError::InvalidPayload {
                                detail: "developer/message tool-addition must omit inline tool \
                                         definitions"
                                    .to_owned(),
                            });
                        }
                    }
                }
            }
            let header_seq = data.get("headerSeq");
            if has_additions {
                if safe_unsigned(header_seq).is_none() {
                    return Err(WireError::InvalidPayload {
                        detail:
                            "developer/message requires headerSeq exactly when tool additions are \
                                 present"
                                .to_owned(),
                    });
                }
            } else if header_seq.is_some() {
                return Err(WireError::InvalidPayload {
                    detail: "developer/message requires headerSeq exactly when tool additions are \
                             present"
                        .to_owned(),
                });
            }
            Ok(())
        }
        "request/header" => {
            if !data.is_object() {
                return Err(WireError::InvalidPayload {
                    detail: "request/header data must be an object".to_owned(),
                });
            }
            let header = data.get("header").filter(|value| value.is_object());
            let Some(header) = header else {
                return Err(WireError::InvalidPayload {
                    detail: "request/header header must be an object".to_owned(),
                });
            };
            if header.get("system").is_some() {
                return Err(WireError::InvalidPayload {
                    detail: "request/header must omit header.system; use system/message".to_owned(),
                });
            }
            if header
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                return Err(WireError::InvalidPayload {
                    detail: "request/header must omit empty tools".to_owned(),
                });
            }
            if header
                .get("adapterDefaults")
                .and_then(Value::as_object)
                .is_some_and(serde_json::Map::is_empty)
            {
                return Err(WireError::InvalidPayload {
                    detail: "request/header must omit empty adapterDefaults".to_owned(),
                });
            }
            Ok(())
        }
        "tool/result" => {
            if !data.is_object() {
                return Err(WireError::InvalidPayload {
                    detail: "tool/result data must be an object".to_owned(),
                });
            }
            if data.get("error").is_none() {
                return Ok(());
            }
            let marks_error = data
                .get("message")
                .and_then(|message| message.get("isError"))
                .and_then(Value::as_bool);
            if marks_error == Some(true) {
                Ok(())
            } else {
                Err(WireError::InvalidPayload {
                    detail: "tool/result error requires message.isError === true".to_owned(),
                })
            }
        }
        // `user/message` and every other surface type only carry the role/block rules above when they
        // are the developer case; a non-developer message with a tool-change block is refused by the
        // generic check below.
        _ => {
            if !is_surface_eligible_type(type_name) {
                return Ok(());
            }
            let message = if type_name == "user/message" {
                Some(data)
            } else {
                data.get("message")
            }
            .filter(|value| value.is_object());
            let Some(message) = message else {
                return Ok(());
            };
            if message.get("role").and_then(Value::as_str) == Some("developer") {
                return Err(WireError::InvalidPayload {
                    detail: "session wire event developer/message and developer role must occur \
                             together"
                        .to_owned(),
                });
            }
            let changes = message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        matches!(
                            block.get("type").and_then(Value::as_str),
                            Some("tool-addition" | "tool-removal")
                        )
                    })
                });
            if changes {
                return Err(WireError::InvalidPayload {
                    detail: format!("{type_name} tool-change blocks require the developer role"),
                });
            }
            Ok(())
        }
    }
}

/// Reads a non-negative safe integer, rejecting `-0`.
///
/// `-0` matters: upstream's check is
/// `Number.isSafeInteger(seq) && seq >= 0 && !Object.is(seq, -0)`. The comparison accepts `-0`, so the
/// `Object.is` clause is the only thing rejecting it, and it is a producer bug worth surfacing rather
/// than coercing. `serde_json`'s `arbitrary_precision` keeps `-0` as a token while `as_i64` reports
/// `Some(0)`, so the check has to read the token.
fn safe_unsigned(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if is_negative_zero(value) {
        return None;
    }
    let number = value.as_number()?;
    if number.is_f64() {
        let float = safe_integral(number.as_f64()?)?;
        return if float >= 0.0 {
            Some(truncate_unsigned(float))
        } else {
            None
        };
    }
    let integer = number.as_i64()?;
    u64::try_from(integer)
        .ok()
        .filter(|value| *value <= MAX_SAFE_INTEGER)
}

/// Reads a safe integer of either sign.
fn safe_signed(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if is_negative_zero(value) {
        return Some(0);
    }
    let number = value.as_number()?;
    if number.is_f64() {
        return Some(truncate_signed(safe_integral(number.as_f64()?)?));
    }
    number
        .as_i64()
        .filter(|value| value.saturating_abs() <= 9_007_199_254_740_991)
}

/// Returns the float when it is integral and inside the safe-integer range.
fn safe_integral(float: f64) -> Option<f64> {
    if float.fract() != 0.0 || float.abs() > 9_007_199_254_740_991.0 {
        return None;
    }
    Some(float)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value was range-checked against the safe-integer bound immediately before"
)]
fn truncate_unsigned(float: f64) -> u64 {
    float as u64
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the value was range-checked against the safe-integer bound immediately before"
)]
fn truncate_signed(float: f64) -> i64 {
    float as i64
}

/// Whether a JSON number is the token `-0` (or `-0.0`, `-0e5`, …).
fn is_negative_zero(value: &Value) -> bool {
    matches!(value, Value::Number(number)
        if number.to_string().starts_with("-0")
            && number.as_f64().is_some_and(|float| float == 0.0))
}

// ---------------------------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------------------------

/// One accepted page of history records.
///
/// Upstream's `SessionPage`. A page is accepted whole: records, then `hasMore`. Accepting up to the
/// first bad record would publish a window that ends early while still reporting the peer's
/// `has_more`, which strands every row in between.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WirePage {
    /// The accepted records, in order.
    pub records: Vec<WireRecord>,
    /// Whether older history remains.
    pub has_more: bool,
}

impl WirePage {
    /// Accepts one page, refusing any record it cannot validate.
    ///
    /// # Errors
    ///
    /// [`WireError::NotAnObject`] or [`WireError::MissingData`] for a malformed page, and every
    /// [`WireError`] [`WireRecord::accept`] returns for a record.
    pub fn accept(value: &Value) -> Result<Self, WireError> {
        let object = value.as_object().ok_or(WireError::NotAnObject)?;
        let records = object
            .get("records")
            .and_then(Value::as_array)
            .ok_or(WireError::MissingData)?;
        let mut accepted = Vec::with_capacity(records.len());
        for record in records {
            accepted.push(WireRecord::accept(record)?);
        }
        let has_more = object
            .get("hasMore")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(Self {
            records: accepted,
            has_more,
        })
    }

    /// The accepted entries, as upstream's `historyEntries` returns them.
    ///
    /// Upstream is a no-op cast because `SessionHistoryRecord` and `SessionEventLikeEntry` are the same
    /// shape; here it is a borrow, which is the same statement with the type system checking it.
    #[must_use]
    pub fn entries(&self) -> &[WireRecord] {
        &self.records
    }

    /// The inclusive last sequence this page covers, or `None` when it is empty.
    #[must_use]
    pub fn tail_cursor(&self) -> Option<u64> {
        self.records.last().map(WireRecord::last_seq)
    }

    /// Finds the first record at or past `seq`, which is where a prepend has to be cut.
    #[must_use]
    pub fn index_from(&self, seq: u64) -> usize {
        self.records
            .iter()
            .position(|record| record.first_seq() >= seq)
            .unwrap_or(self.records.len())
    }
}

// ---------------------------------------------------------------------------------------------
// Page requests
// ---------------------------------------------------------------------------------------------

/// Pagination fields bound to an already-addressed Session journal.
///
/// Upstream's `ClientSessionPageRequest` is `Omit<SessionPageRequest, 'address'|'throughSeq'>`: the
/// address is fixed by the stream, and `throughSeq` is decided per call by the cursor algebra rather
/// than by the caller. Modelling the omission as an absent field — instead of an `Option` a caller
/// could fill in — is what keeps a repair read from silently reusing a stale cut.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientPageRequest {
    /// Exclusive upper bound of an older page.
    pub before_seq: Option<u64>,
    /// Message budget; omitted means the transport default.
    pub max_messages: Option<usize>,
    /// Turn-boundary stop rule.
    pub turn_window: Option<TurnWindow>,
}

impl ClientPageRequest {
    /// A request with no bounds: the tail page the opening follow uses.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            before_seq: None,
            max_messages: None,
            turn_window: None,
        }
    }

    /// The request used for reconnect and gap repair.
    ///
    /// Upstream `repairRequest` keeps `maxMessages` and `turnWindow` and rebuilds the rest. Read
    /// literally that keeps `beforeSeq` only when the input had none, so this reproduces upstream by
    /// dropping the *bound*: a repair must re-read the tail up to the required cursor, and a residual
    /// `beforeSeq` would cut that read short so the repair could never reach the cursor it is
    /// repairing.
    #[must_use]
    pub fn repair_request(self) -> Self {
        Self {
            before_seq: None,
            max_messages: self.max_messages,
            turn_window: self.turn_window,
        }
    }

    /// Whether this request's window fields can be honoured.
    ///
    /// # Errors
    ///
    /// [`HistoryError`] with `gateway/bad-request` for a zero budget, or for a turn window whose
    /// message minimum exceeds the budget — a window demanding more messages than the budget allows
    /// would stop only at exhaustion, which is not a window.
    pub fn validate(self) -> Result<Self, HistoryError> {
        if self.max_messages == Some(0) {
            return Err(HistoryError::bad_request(
                "maxMessages must be a positive safe integer",
            ));
        }
        if let (Some(window), Some(max)) = (self.turn_window, self.max_messages) {
            if window.min_messages > max {
                return Err(HistoryError::bad_request(format!(
                    "turnWindow.minMessages ({}) must not exceed maxMessages ({max})",
                    window.min_messages
                )));
            }
        }
        if let Some(window) = self.turn_window {
            if window.min_messages == 0 {
                return Err(HistoryError::bad_request(
                    "turnWindow.minMessages must be a positive safe integer no greater than \
                     maxMessages",
                ));
            }
            if window.min_turns == 0 {
                return Err(HistoryError::bad_request(
                    "turnWindow.minTurns must be a positive safe integer",
                ));
            }
        }
        Ok(self)
    }
}

/// Whether a follow opening asks for Assistant notifications and session activation.
///
/// Upstream always opts into both on the follow path. The flags exist here so
/// [`JournalStream`]'s baseline rule has something to key on, and so a caller that only wants durable
/// rows can say so instead of being refused a baseline it never asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FollowOptions {
    /// Whether the opening must carry an Assistant baseline.
    pub assistant_stream: bool,
    /// Whether the Host should activate the session as part of the opening.
    pub activate: bool,
}

impl FollowOptions {
    /// The upstream default: activate and opt into Assistant notifications.
    #[must_use]
    pub const fn upstream_default() -> Self {
        Self {
            assistant_stream: true,
            activate: true,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Assistant-stream continuity
// ---------------------------------------------------------------------------------------------

/// What one Assistant frame did to the revision counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevisionStep {
    /// The frame continues the lifecycle; the revision advanced.
    Continue,
    /// The frame skipped a revision, so the transient picture must be rebuilt.
    Rebaseline {
        /// The revision that was expected.
        expected: u64,
        /// The revision that arrived.
        received: u64,
    },
}

/// The dense revision counter over one process-local Assistant lifecycle.
///
/// Before an opening baseline arrives there is no base, so [`AssistantRevision::observe`] is only
/// meaningful after [`AssistantRevision::opened_at`]. Every later frame must be exactly
/// `previous + 1`; anything else means the carrier dropped a frame and the accumulated stream is
/// missing a chunk, so the only correct response is to re-open and rebuild from the new baseline. The
/// durable cursor is deliberately untouched by that recovery: nothing durable was lost, which is why
/// upstream publishes `carrierFailed` rather than `failed`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AssistantRevision {
    revision: u64,
    opened: bool,
}

impl AssistantRevision {
    /// A counter with no opening baseline yet.
    #[must_use]
    pub const fn unopened() -> Self {
        Self {
            revision: 0,
            opened: false,
        }
    }

    /// A counter seeded by an opening baseline's revision.
    #[must_use]
    pub const fn opened_at(revision: u64) -> Self {
        Self {
            revision,
            opened: true,
        }
    }

    /// The current revision.
    #[must_use]
    pub const fn revision(self) -> u64 {
        self.revision
    }

    /// Whether an opening baseline has been accepted.
    #[must_use]
    pub const fn is_opened(self) -> bool {
        self.opened
    }

    /// The revision the next frame must carry.
    #[must_use]
    pub const fn expected(self) -> u64 {
        self.revision.saturating_add(1)
    }

    /// Applies one Assistant frame's revision.
    ///
    /// A skipped revision is reported, not accepted: treating a gap as continuity would splice a
    /// chunk onto a stream missing its predecessor, and the result would look like a normal answer
    /// with a paragraph missing.
    pub fn observe(&mut self, revision: u64) -> RevisionStep {
        let expected = self.expected();
        if revision == expected {
            self.revision = revision;
            RevisionStep::Continue
        } else {
            RevisionStep::Rebaseline {
                expected,
                received: revision,
            }
        }
    }
}

/// One Assistant frame's shape, as far as the transport cares.
///
/// Upstream discriminates `start`/`chunk`/`end` and reads only `revision` off them here; the payload
/// belongs to [`ClientAssistantStream`](girder_core::session::window), and duplicating that fold
/// would mean two places to keep the chunk algebra in sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssistantFrame {
    /// The frame's revision.
    pub revision: u64,
}

impl AssistantFrame {
    /// Reads one Assistant frame off the wire.
    ///
    /// # Errors
    ///
    /// [`StreamError::MalformedAssistant`] when the value is not an object with a known `type` and a
    /// non-negative safe-integer `revision`.
    pub fn accept(value: &Value) -> Result<Self, StreamError> {
        let object = value
            .as_object()
            .ok_or_else(|| StreamError::MalformedAssistant {
                detail: "frame must be an object".to_owned(),
            })?;
        let kind = object.get("type").and_then(Value::as_str).ok_or_else(|| {
            StreamError::MalformedAssistant {
                detail: "frame has no type".to_owned(),
            }
        })?;
        if !matches!(kind, "start" | "chunk" | "end") {
            return Err(StreamError::MalformedAssistant {
                detail: format!("unknown frame type {kind}"),
            });
        }
        let revision = safe_unsigned(object.get("revision")).ok_or_else(|| {
            StreamError::MalformedAssistant {
                detail: "revision must be a non-negative safe integer".to_owned(),
            }
        })?;
        Ok(Self { revision })
    }
}

// ---------------------------------------------------------------------------------------------
// Follow frames
// ---------------------------------------------------------------------------------------------

/// One frame of an addressed Session event journal.
#[derive(Clone, Debug, PartialEq)]
pub enum FollowFrame {
    /// A complete opening window with the cut it covers.
    Snapshot {
        /// The announced cut, or `None` when the journal is empty.
        ///
        /// Upstream announces `-1` for an empty journal and a real seq otherwise; the two states
        /// cannot be collapsed, because seq `0` is a real event in any non-empty log.
        cursor: Option<u64>,
        /// The records at that cut.
        page: WirePage,
        /// The opted-in Assistant baseline's revision, when one arrived.
        assistant_revision: Option<u64>,
    },
    /// One live durable event.
    Entry(WireRecord),
    /// One live Assistant frame.
    Assistant(AssistantFrame),
}

/// What accepting a follow frame produced.
#[derive(Clone, Debug, PartialEq)]
pub enum FollowChange {
    /// The window was replaced from an opening snapshot.
    Replace {
        /// The page at the announced cursor.
        page: WirePage,
    },
    /// One live durable entry extended the window.
    Append {
        /// The appended record.
        record: WireRecord,
    },
    /// One Assistant frame was published as a cursorless change.
    ///
    /// Upstream calls this Gateway change a `notification` and Session change an `assistant-stream`;
    /// this is that rename, and it is the only structural difference between the two unions.
    AssistantStream {
        /// The frame.
        frame: AssistantFrame,
    },
}

/// The address-bound event-journal state machine.
///
/// [`SessionEventStream`](girder_core::session::controller) upstream also owns a socket. Every
/// decision it makes, however, is a function of the frames it sees, and those decisions are where the
/// bugs live; a caller feeds frames in and receives either a change to publish or a typed refusal.
#[derive(Clone, Debug)]
pub struct JournalStream {
    name: String,
    options: FollowOptions,
    opened: bool,
    assistant: AssistantRevision,
    cursor: Option<u64>,
}

impl JournalStream {
    /// A journal stream for one already-addressed session.
    #[must_use]
    pub fn new(name: impl Into<String>, options: FollowOptions) -> Self {
        Self {
            name: name.into(),
            options,
            opened: false,
            assistant: AssistantRevision::unopened(),
            cursor: None,
        }
    }

    /// The diagnostic name, which every error message quotes.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The options this stream was opened with.
    #[must_use]
    pub const fn options(&self) -> FollowOptions {
        self.options
    }

    /// Whether an opening snapshot has been accepted.
    #[must_use]
    pub const fn is_opened(&self) -> bool {
        self.opened
    }

    /// The announced cursor of the current generation, once opened.
    #[must_use]
    pub const fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    /// The Assistant revision counter.
    #[must_use]
    pub const fn assistant_revision(&self) -> AssistantRevision {
        self.assistant
    }

    /// Accepts one frame.
    ///
    /// # Errors
    ///
    /// - [`StreamError::NotOpen`] for a live frame before any opening snapshot: its position is
    ///   undefined because there is no established cut to compare against.
    /// - [`StreamError::RepeatedOpening`] for a second snapshot in one generation.
    /// - [`StreamError::MissingAssistantBaseline`] when the opening opted into Assistant
    ///   notifications but carried no baseline.
    /// - [`StreamError::CursorMismatch`] when the snapshot's records do not end at its announced
    ///   cursor.
    /// - [`StreamError::StaleEntry`] for a live entry that does not follow the last applied cursor.
    /// - [`StreamError::RevisionGap`] for an Assistant frame that skipped a revision.
    pub fn accept(&mut self, frame: FollowFrame) -> Result<FollowChange, StreamError> {
        match frame {
            FollowFrame::Snapshot {
                cursor,
                page,
                assistant_revision,
            } => {
                if self.opened {
                    return Err(StreamError::RepeatedOpening {
                        name: self.name.clone(),
                    });
                }
                if page.tail_cursor() != cursor {
                    return Err(StreamError::CursorMismatch {
                        name: self.name.clone(),
                        announced: cursor,
                        actual: page.tail_cursor(),
                    });
                }
                if let Some(revision) = assistant_revision {
                    self.assistant = AssistantRevision::opened_at(revision);
                } else {
                    if self.options.assistant_stream {
                        return Err(StreamError::MissingAssistantBaseline {
                            name: self.name.clone(),
                        });
                    }
                    self.assistant = AssistantRevision::unopened();
                }
                self.cursor = cursor;
                self.opened = true;
                Ok(FollowChange::Replace { page })
            }
            FollowFrame::Entry(record) => {
                if !self.opened {
                    return Err(StreamError::NotOpen {
                        name: self.name.clone(),
                    });
                }
                if let Some(cursor) = self.cursor {
                    if record.last_seq() <= cursor {
                        return Err(StreamError::StaleEntry {
                            name: self.name.clone(),
                            cursor,
                            seq: record.last_seq(),
                        });
                    }
                }
                self.cursor = Some(record.last_seq());
                Ok(FollowChange::Append { record })
            }
            FollowFrame::Assistant(frame) => {
                if !self.assistant.is_opened() {
                    return Err(StreamError::NotOpen {
                        name: self.name.clone(),
                    });
                }
                match self.assistant.observe(frame.revision) {
                    RevisionStep::Continue => Ok(FollowChange::AssistantStream { frame }),
                    RevisionStep::Rebaseline { expected, received } => {
                        Err(StreamError::RevisionGap {
                            name: self.name.clone(),
                            expected,
                            received,
                        })
                    }
                }
            }
        }
    }

    /// Records that the carrier lost a generation, so the next frame must be a new opening.
    ///
    /// The published window and the durable cursor survive: a reconnect re-reads the same journal, and
    /// discarding the cursor would make the replacement page look like a gap. Only the *opening*
    /// expectation resets.
    pub fn reconnect(&mut self) {
        self.opened = false;
    }
}

/// Why a follow frame was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamError {
    /// A live frame arrived before the opening snapshot.
    NotOpen {
        /// The stream's diagnostic name.
        name: String,
    },
    /// A second opening snapshot arrived in one generation.
    RepeatedOpening {
        /// The stream's diagnostic name.
        name: String,
    },
    /// The opening opted into Assistant notifications but carried no baseline.
    MissingAssistantBaseline {
        /// The stream's diagnostic name.
        name: String,
    },
    /// The opening page did not end at its announced cursor.
    CursorMismatch {
        /// The stream's diagnostic name.
        name: String,
        /// The announced cursor, `None` when the journal was announced empty.
        announced: Option<u64>,
        /// Where the records actually ended.
        actual: Option<u64>,
    },
    /// A live entry did not follow the last applied cursor.
    StaleEntry {
        /// The stream's diagnostic name.
        name: String,
        /// The last applied cursor.
        cursor: u64,
        /// The entry's cursor.
        seq: u64,
    },
    /// An Assistant frame skipped a revision.
    RevisionGap {
        /// The stream's diagnostic name.
        name: String,
        /// The revision that was expected.
        expected: u64,
        /// The revision that arrived.
        received: u64,
    },
    /// An Assistant frame's payload was not the shape the transport needs.
    MalformedAssistant {
        /// What is wrong.
        detail: String,
    },
}

impl StreamError {
    /// Whether the caller should re-open instead of surfacing a terminal error.
    ///
    /// Only a revision gap is recoverable: it means transient frames were lost, which a new generation
    /// fixes. Everything else is a protocol disagreement that retrying cannot resolve.
    #[must_use]
    pub const fn is_recoverable(&self) -> bool {
        matches!(self, Self::RevisionGap { .. })
    }

    /// The carrier-failure message upstream publishes for the recoverable case.
    ///
    /// `None` for terminal errors, whose own message is the report.
    #[must_use]
    pub fn carrier_message(&self) -> Option<String> {
        match self {
            Self::RevisionGap { expected, .. } => Some(format!(
                "session assistant stream skipped revision {expected}"
            )),
            _ => None,
        }
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotOpen { name } => {
                write!(
                    formatter,
                    "{name} emitted an entry before its opening cursor"
                )
            }
            Self::RepeatedOpening { name } => {
                write!(formatter, "{name} emitted more than one opening cursor")
            }
            Self::MissingAssistantBaseline { name } => write!(
                formatter,
                "{name} omitted its opted-in opening Assistant baseline"
            ),
            Self::CursorMismatch {
                name,
                announced,
                actual,
            } => match (announced, actual) {
                (Some(announced), Some(actual)) => write!(
                    formatter,
                    "{name} announced cursor {announced} but its page ends at {actual}"
                ),
                (Some(announced), None) => write!(
                    formatter,
                    "{name} announced cursor {announced} but its page is empty"
                ),
                (None, Some(actual)) => write!(
                    formatter,
                    "{name} announced an empty journal but its page ends at {actual}"
                ),
                (None, None) => write!(formatter, "{name} announced an inconsistent empty journal"),
            },
            Self::StaleEntry { name, cursor, seq } => write!(
                formatter,
                "{name} emitted entry {seq} at or behind its cursor {cursor}"
            ),
            Self::RevisionGap { expected, .. } => {
                write!(
                    formatter,
                    "session assistant stream skipped revision {expected}"
                )
            }
            Self::MalformedAssistant { detail } => {
                write!(formatter, "session assistant frame is malformed: {detail}")
            }
        }
    }
}

impl std::error::Error for StreamError {}

// ---------------------------------------------------------------------------------------------
// Control stream
// ---------------------------------------------------------------------------------------------

/// What the control stream told the domain to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlAction {
    /// Replace the whole projection model from a complete baseline.
    Replace,
    /// Apply one incremental update.
    Update,
}

/// The Host-wide Session control stream's decision half.
///
/// `createSessionControlStream` without the socket. Two rules make it more than a pass-through, and
/// both are what a reconnecting client is built on:
///
/// 1. **Exactly one baseline per generation.** A second baseline means the Host changed the model
///    mid-generation, so every following delta's base is unknown.
/// 2. **No delta before the baseline.** A delta against a model from a *different* generation is not
///    an update but a corruption, because the two generations were not necessarily related.
#[derive(Clone, Debug)]
pub struct ControlStream {
    name: String,
    generation: u64,
    baseline_seen: bool,
    replaced: u64,
    updated: u64,
}

impl ControlStream {
    /// A control stream in its first generation.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            generation: 0,
            baseline_seen: false,
            replaced: 0,
            updated: 0,
        }
    }

    /// The current physical generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// How many baselines were accepted.
    #[must_use]
    pub const fn replaced(&self) -> u64 {
        self.replaced
    }

    /// How many deltas were accepted.
    #[must_use]
    pub const fn updated(&self) -> u64 {
        self.updated
    }

    /// Whether the current generation has delivered its baseline.
    #[must_use]
    pub const fn baseline_seen(&self) -> bool {
        self.baseline_seen
    }

    /// Starts a new physical generation, which expects a fresh baseline.
    pub fn advance(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.baseline_seen = false;
    }

    /// Classifies one control frame.
    ///
    /// # Errors
    ///
    /// [`ControlError::DeltaBeforeBaseline`] for a delta in a generation that has not delivered its
    /// baseline, and [`ControlError::RepeatedBaseline`] for a second baseline in one generation.
    pub fn accept(&mut self, frame: &ControlFrame) -> Result<ControlAction, ControlError> {
        match frame {
            ControlFrame::Baseline(ControlBaseline { .. }) => {
                if self.baseline_seen {
                    return Err(ControlError::RepeatedBaseline {
                        name: self.name.clone(),
                    });
                }
                self.baseline_seen = true;
                self.replaced = self.replaced.saturating_add(1);
                Ok(ControlAction::Replace)
            }
            ControlFrame::Projection(_) => {
                if !self.baseline_seen {
                    return Err(ControlError::DeltaBeforeBaseline {
                        name: self.name.clone(),
                    });
                }
                self.updated = self.updated.saturating_add(1);
                Ok(ControlAction::Update)
            }
        }
    }
}

/// Why a control frame was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlError {
    /// A delta arrived before its generation's baseline.
    DeltaBeforeBaseline {
        /// The stream's diagnostic name.
        name: String,
    },
    /// A generation delivered more than one baseline.
    RepeatedBaseline {
        /// The stream's diagnostic name.
        name: String,
    },
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeltaBeforeBaseline { name } => {
                write!(
                    formatter,
                    "{name} emitted an update before its opening snapshot"
                )
            }
            Self::RepeatedBaseline { name } => {
                write!(formatter, "{name} emitted more than one opening snapshot")
            }
        }
    }
}

impl std::error::Error for ControlError {}

/// How an ended control generation should be classified.
///
/// Upstream's `ended: accepted => accepted ? carrier error : protocol error`. The distinction is the
/// difference between "reconnect quietly" and "surface a broken Host": a generation that ended *after*
/// delivering its baseline lost a carrier, and one that ended before never spoke the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndKind {
    /// The generation delivered its baseline and then lost its carrier.
    CarrierLost,
    /// The generation ended before its opening baseline.
    ProtocolViolation,
}

impl EndKind {
    /// Classifies a generation end from whether its opening baseline was accepted.
    #[must_use]
    pub const fn classify(baseline_accepted: bool) -> Self {
        if baseline_accepted {
            Self::CarrierLost
        } else {
            Self::ProtocolViolation
        }
    }

    /// The message upstream reports for this classification.
    #[must_use]
    pub fn message(self, name: &str) -> String {
        match self {
            Self::CarrierLost => format!("{name} ended without a terminal result"),
            Self::ProtocolViolation => format!("{name} ended before its opening snapshot"),
        }
    }
}
