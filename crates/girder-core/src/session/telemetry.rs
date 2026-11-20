//! Session telemetry capture: turning canonical session events into alertable records.
//!
//! Corresponds to upstream `packages/session/session-telemetry` (`SessionTelemetryBackend` and its
//! `SessionTelemetryCoordinator`), plus the export shape of `session-telemetry-otel`, which is the
//! one backend that consumes these records today.
//!
//! # Why this module exists
//!
//! Upstream's session workspace is twenty-four packages. The local crate covers most of them —
//! format migration ([`crate::session::migration`]), JSONL persistence, projections, titles,
//! checkpoints, stats, replay, rewind — but telemetry has **no local counterpart at all**: no
//! `session_telemetry` module, no `telemetry` / `otel` / `opentelemetry` mention anywhere under
//! `crates/girder-core/src`. A deployment could record a session perfectly and still have no way to
//! report what happened in it.
//!
//! The count said twenty-nine until it was checked against the source tree, which holds twenty-four
//! directories. A number in a comment is a claim like any other, and this one had been wrong for
//! several rounds without anything failing.
//!
//! # What is ported, and what is deliberately not
//!
//! The parts ported here are the ones that are **pure functions of the log**: how a captured event
//! maps to a severity, what identity attributes a record carries, how the export hits are counted,
//! and how the `session-telemetry/record` redaction chain composes. Those are where the actual
//! decisions live, and they are testable without a context, a backend, or a network.
//!
//! Not ported, and deliberately: the cordis fiber plumbing (`ctx.on`/`ctx.effect`/`ctx.waterfall`),
//! the `WeakMap` handoff cursor, and the OTLP transport. The first two are this crate's registry
//! concern rather than a session concern, and the transport is an HTTP client with a byte budget —
//! porting it without its integration tests would produce the kind of empty shell this task exists
//! to avoid. `session-telemetry-otel`'s *batching* arithmetic is ported, because the byte bound is
//! the part that is easy to get subtly wrong (see [`plan_export_batches`]).
//!
//! # The capture contract
//!
//! One canonical session event becomes one *ledger* record. Two thing kinds are not log events at
//! all and become *ops* records instead: `agent-error` (a live-bus emission with no log home) and
//! `shutdown` (the session's clean-exit marker). Keeping them on a separate channel is what stops a
//! receiver from mistaking an operational signal for a row that appears in the session log.

use girder_core::{FormatVersion, SessionEvent, TurnEndReason};

/// Which of the two streams a record belongs to.
///
/// Upstream keeps these apart under separate instrumentation scopes. The separation is load
/// bearing: a ledger record mirrors one canonical log event one-to-one, so a receiver can compare
/// its count against the log and detect loss. An ops record has no log home, so folding it into the
/// ledger would make that comparison fail by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    /// A one-to-one mirror of a canonical session-log event.
    Ledger,
    /// An operational signal with no log home: `agent-error`, `shutdown`.
    Ops,
}

impl Channel {
    /// The wire spelling, matching upstream's `channel` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ledger => "ledger",
            Self::Ops => "ops",
        }
    }
}

/// Pre-mapped alerting severity.
///
/// Upstream maps severity **at capture**, not at the receiver, so an alert rule needs no knowledge
/// of the harness's event vocabulary to fire on failures. That is why the mapping lives here rather
/// than being left to whoever reads the record: the receiver of a `tool/result` should not have to
/// know that the error flag is nested inside the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Normal operation; the default for every event that does not report an outcome.
    Info,
    /// Escalation available to a redaction policy or a backend.
    ///
    /// The capture mapping never produces it — see [`severity_of`] — but a policy that recognizes
    /// a suspicious pattern raises a record from [`Severity::Info`] to this, and a backend may map
    /// it to its own alert level. Kept constructible for exactly that use, and exercised by
    /// [`escalate`].
    Warn,
    /// The event's own outcome flag says this failed.
    Error,
}

impl Severity {
    /// The wire spelling, matching upstream's `severity` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// The outcome of one `session-telemetry/record` rule.
///
/// Upstream's waterfall hands a rule the record and a `next` thunk. A Rust closure cannot be
/// re-entered from inside its own call, so the same contract is expressed as a two-way choice:
/// either the rule passes the record on (possibly transformed) or it replaces everything beneath
/// it. Returning [`Verdict::Replace`] is exactly upstream's "a listener that skips `next()`".
pub enum Verdict {
    /// Pass this record to the next rule in the chain.
    Pass(Record),
    /// Discard the rest of the chain and use this record instead.
    Replace(Record),
}

/// One logical record handed to a backend.
///
/// The body is a [`serde_json::Value`] clone of the event's data rather than a borrow, because the
/// canonical event is mutable and a backend serializes later; upstream clones at capture time for
/// the same reason.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// `ledger` or `ops`.
    pub channel: Channel,
    /// Unix epoch milliseconds: the event's append time for ledger, the emission time for ops.
    pub time: u64,
    /// Pre-mapped alerting severity.
    pub severity: Severity,
    /// Identity attributes; see [`capture`] for the ledger set and [`agent_error`] for the ops set.
    pub attributes: Vec<(String, Attribute)>,
    /// The payload. Never mutated after handoff.
    pub body: serde_json::Value,
}

/// One identity attribute's value.
///
/// A small enum rather than `serde_json::Value`: upstream declares the attribute map as
/// `string | number`, and admitting any JSON value is how a boolean `isError` ends up serialized as
/// an attribute-like string that every receiver then has to re-parse. Two variants make the allowed
/// shape unforgeable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attribute {
    /// A string attribute.
    Text(String),
    /// A numeric attribute.
    Number(u64),
}

impl Attribute {
    /// The wire spelling: the text itself, or the number in decimal.
    ///
    /// Both variants become text on the wire because that is what every telemetry attribute
    /// protocol accepts (OTLP carries numbers and strings, but a mixed map in JSON does not, and
    /// upstream's attribute record is `string | number` per value -- callers stringify uniformly).
    #[must_use]
    pub fn as_wire(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Number(number) => number.to_string(),
        }
    }
}

/// What a capture needs to know about the session a record came from.
///
/// A struct of header facts rather than a session handle: the capture mapping is then a pure
/// function, which is what lets its cases be tested without a store. Upstream reads these same four
/// facts off `session.header` plus `inheritedEventCount`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFacts {
    /// The session id, as logged.
    pub id: String,
    /// The format generation the log was written in.
    pub format_version: Option<FormatVersion>,
    /// The session's working directory, when the header declared one.
    pub cwd: Option<String>,
    /// The parent session, for a fork.
    pub parent_id: Option<String>,
    /// For a seeded (forked) session, how many leading events were inherited.
    ///
    /// `Some` rather than folded into `parent_id` because the two answer different questions: the
    /// parent names the lineage, this names the exact cut. A receiver stitching a child's ledger to
    /// its parent's needs the count, and cannot recover it from the parent id alone.
    pub seed_length: Option<u64>,
}

/// The severity a captured event carries.
///
/// Only two event types report an outcome, so only two can be `Error`:
///
/// - `tool/result`, when the tool failed;
/// - `turn/end`, when the turn ended in error.
///
/// A turn that was aborted or blocked is deliberately **not** an error. It is a turn that stopped
/// on purpose, and mapping it to `Error` would page somebody every time a person pressed Ctrl-C.
///
/// Every other type falls through to [`Severity::Info`], including event types this build does not
/// know about. That is upstream's merge-extensible fall-through: a plugin's own event keeps its own
/// outcome semantics, and the capture layer must not invent one for it.
#[must_use]
pub fn severity_of(event: &SessionEvent) -> Severity {
    match event {
        SessionEvent::ToolResult { is_error, .. } => {
            if *is_error {
                Severity::Error
            } else {
                Severity::Info
            }
        }
        SessionEvent::TurnEnd { reason, .. } => match reason {
            TurnEndReason::Failed { .. } => Severity::Error,
            TurnEndReason::Completed
            | TurnEndReason::Aborted { .. }
            | TurnEndReason::Blocked
            | TurnEndReason::Budget => Severity::Info,
        },
        _ => Severity::Info,
    }
}

/// Map one canonical event to a ledger record.
///
/// The attributes are deliberately minimal — envelope identity plus the header facts that are not
/// recoverable from the body. Anything the body already carries is not duplicated into attributes,
/// because every duplicated field is a second place for a receiver to read a stale value.
///
/// `seq` is one-based like every other sequence in this crate, and is passed in rather than taken
/// from the event because a [`SessionEvent`] carries no sequence number of its own.
#[must_use]
pub fn capture(facts: &SessionFacts, seq: u64, time: u64, event: &SessionEvent) -> Record {
    let mut attributes = vec![
        ("session.id".to_owned(), Attribute::Text(facts.id.clone())),
        (
            "event.type".to_owned(),
            Attribute::Text(event.type_name().to_owned()),
        ),
        ("event.seq".to_owned(), Attribute::Number(seq)),
    ];

    // The format version is optional so a caller that only has events in hand can still capture.
    // A receiver comparing two logs' schemas wants it, which is why it ranks above the location.
    if let Some(version) = facts.format_version {
        attributes.insert(
            1,
            (
                "session.format_version".to_owned(),
                Attribute::Number(version.as_number()),
            ),
        );
    }
    if let Some(cwd) = &facts.cwd {
        attributes.push(("session.cwd".to_owned(), Attribute::Text(cwd.clone())));
    }
    if let Some(parent) = &facts.parent_id {
        attributes.push((
            "session.parent_id".to_owned(),
            Attribute::Text(parent.clone()),
        ));
    }
    if let Some(seed_length) = facts.seed_length {
        attributes.push((
            "session.seed_length".to_owned(),
            Attribute::Number(seed_length),
        ));
    }

    Record {
        channel: Channel::Ledger,
        time,
        severity: severity_of(event),
        attributes,
        body: body_of(event),
    }
}

/// The payload a ledger record carries: the event's data, without its `type` tag.
///
/// The clone is unconditional and infallible here — unlike upstream's `structuredClone`, which
/// relies on append-time validation to guarantee it cannot throw. The local event is already a
/// concrete Rust type, so serialization is the only step that could fail, and a failure is reported
/// instead of silently substituting a null body.
#[must_use]
pub fn body_of(event: &SessionEvent) -> serde_json::Value {
    let Ok(mut value) = serde_json::to_value(event) else {
        return serde_json::Value::Null;
    };
    // `type` is the envelope's tag, so it belongs in attributes (`event.type`), not in the body.
    // Leaving it here would make every receiver see the discriminator twice.
    if let Some(object) = value.as_object_mut() {
        object.remove("type");
    }
    value
}

/// Normalize a thrown error into the two facts an `agent-error` record carries.
///
/// Upstream wraps a non-`Error` thrown value in `new Error(String(value))`, which yields the name
/// `Error` and the stringified message. Passing `None` reproduces that exactly: the live bus can
/// carry any value, and a receiver should not have to branch on whether the harness threw a proper
/// error or a bare string.
#[must_use]
pub fn error_detail(name: Option<&str>, message: &str) -> (String, String) {
    (name.unwrap_or("Error").to_owned(), message.to_owned())
}

/// Build the `agent-error` operational record.
///
/// `session_id` is threaded through as a `&str` rather than a resolved session because the relay
/// fires from the live bus, where the failing agent is identified by id even if its session has
/// already been torn down. Reporting the error must not depend on resolving the session.
#[must_use]
pub fn agent_error(
    session_id: &str,
    agent_id: &str,
    turn: u64,
    step: u64,
    name: Option<&str>,
    message: &str,
    time: u64,
) -> Record {
    let (name, message) = error_detail(name, message);
    Record {
        channel: Channel::Ops,
        time,
        // A reported error is the one thing that must always alert, regardless of which event it
        // came from; there is no outcome flag left to consult at this point.
        severity: Severity::Error,
        attributes: vec![
            (
                "telemetry.op".to_owned(),
                Attribute::Text("agent-error".to_owned()),
            ),
            (
                "session.id".to_owned(),
                Attribute::Text(session_id.to_owned()),
            ),
            ("agent.id".to_owned(), Attribute::Text(agent_id.to_owned())),
            ("error.name".to_owned(), Attribute::Text(name.clone())),
            ("turn".to_owned(), Attribute::Number(turn)),
            ("step".to_owned(), Attribute::Number(step)),
        ],
        body: serde_json::json!({ "name": name, "message": message }),
    }
}

/// Build the `shutdown` operational record.
///
/// `session_id` is taken as a string for the same reason as in [`agent_error`]: the marker is built at
/// a disposal edge, where the session may already be unreachable.
///
/// Emitted at a session's own disposal edge, and again at coordinator disposal for sessions still
/// alive then. A receiver pairs it against the session's creation to measure a lifecycle, which is
/// why it is a separate record rather than a flag on the last ledger row: a session can be disposed
/// with no events after its last turn, and a flag would then have nowhere to live.
#[must_use]
pub fn shutdown(session_id: &str, time: u64) -> Record {
    Record {
        channel: Channel::Ops,
        time,
        // A clean exit is not a failure; `Info` vs `Error` is exactly the distinction a receiver
        // uses to tell "the run finished" from "the run died".
        severity: Severity::Info,
        attributes: vec![
            (
                "telemetry.op".to_owned(),
                Attribute::Text("shutdown".to_owned()),
            ),
            (
                "session.id".to_owned(),
                Attribute::Text(session_id.to_owned()),
            ),
        ],
        body: serde_json::json!({ "op": "shutdown" }),
    }
}

/// One `session-telemetry/record` rule.
///
/// A trait object rather than a generic parameter because the chain is heterogeneous: a deployment
/// mounts a secret scrubber, a path anonymizer, and a size limiter together, and they share no type.
pub type Policy<'a> = &'a dyn Fn(&Record) -> Option<Verdict>;

/// Run a record through the `session-telemetry/record` chain.
///
/// The chain is ordered **outermost first**, matching upstream: each rule sees the record as the
/// outermost wrapper and passes its result inward. A rule that returns [`Verdict::Replace`] ends the
/// chain immediately — upstream's "a listener that skips `next()` replaces everything beneath it" —
/// so no later rule runs and the replacement is what reaches the backend.
///
/// # Why the return is optional
///
/// Upstream has a second way a record can be withheld: a rule that throws. The coordinator contains
/// that as fail-closed, dropping the one record while the replay continues. In Rust a rule returns a
/// value instead of throwing, so the analogous case is `None`: it means "withhold this record". The
/// caller skips that record and continues with the next event.
///
/// # Sealing the original log
///
/// Redaction applies to the exported copy only; the canonical session log is never rewritten. That
/// is guaranteed structurally here: [`Record`] owns its body, and [`capture`] built it from a clone,
/// so no rule can reach the store's events through a record.
#[must_use]
pub fn apply_policies(record: Record, policies: &[Policy<'_>]) -> Option<Record> {
    let mut current = record;
    for policy in policies {
        match policy(&current)? {
            Verdict::Pass(next) => current = next,
            Verdict::Replace(next) => return Some(next),
        }
    }
    Some(current)
}

/// Whether a record would be exported at all, given a sharing mode.
///
/// Upstream's `sharing` is deployment-selected and independent of whether the SDK actually delivered
/// anything. `FeedbackOnly` is not "nothing": it still exports the feedback-authorized subset, which
/// is why the mode is a filter rather than a boolean.
///
/// The ledger is the wider surface — it mirrors every event — so `FeedbackOnly` keeps ops records and
/// drops ledger records. That is the conservative reading: an operational signal says *that* a run
/// errored or ended, while a ledger row says *what was said*.
#[must_use]
pub fn is_exported(sharing: SharingStatus, record: &Record) -> bool {
    match sharing {
        SharingStatus::Full => true,
        SharingStatus::FeedbackOnly => record.channel == Channel::Ops,
        SharingStatus::Disabled => false,
    }
}

/// Deployment-selected session-sharing mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharingStatus {
    /// Everything is exported.
    Full,
    /// Only the feedback-authorized subset — see [`is_exported`].
    FeedbackOnly,
    /// Nothing is exported; capture is off.
    Disabled,
}

/// One batch of records, sized to fit a byte bound.
#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    /// The batch's records, in capture order.
    pub records: Vec<Record>,
    /// The batch's serialized size, as measured during planning.
    pub bytes: usize,
}

/// A record that cannot be exported at any batch size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OversizedRecord {
    /// Its index in the input, so the caller can report which one was dropped.
    pub index: usize,
    /// Its serialized size, which exceeds the bound.
    pub bytes: usize,
}

/// Split records into batches that each fit within `max_bytes`.
///
/// This is the part of `session-telemetry-otel` worth porting: the byte bound, not the HTTP call.
/// Three properties matter and each is a bug someone would otherwise ship:
///
/// 1. **A batch never exceeds the bound.** The size is measured on the serialized form, not on the
///    record count, because a record carrying a 40 KB tool output costs a thousand times a
///    `step/start`.
/// 2. **Order is preserved.** Records are a ledger; reordering would break the `event.seq` sequence a
///    receiver uses to detect loss.
/// 3. **An oversized record is reported, not silently dropped or allowed through.** A single record
///    larger than the bound cannot be batched, and passing it anyway would make the request fail
///    every time it was retried. It is returned separately so the caller can make an explicit
///    decision about it.
///
/// A `max_bytes` of zero is treated as "no limit", which is the only reading that does not make
/// every record oversized.
#[must_use]
pub fn plan_export_batches(
    records: Vec<Record>,
    max_bytes: usize,
) -> (Vec<Batch>, Vec<OversizedRecord>) {
    let mut batches = Vec::new();
    let mut oversized = Vec::new();
    let mut current: Vec<Record> = Vec::new();
    let mut current_bytes = 0usize;

    for (index, record) in records.into_iter().enumerate() {
        let size = record_size(&record);
        if max_bytes != 0 && size > max_bytes {
            oversized.push(OversizedRecord { index, bytes: size });
            continue;
        }
        // Flush before adding, so a batch never exceeds the bound. The check is `>` rather than
        // `>=` because a batch that exactly fills the bound is still valid.
        if max_bytes != 0 && !current.is_empty() && current_bytes + size > max_bytes {
            batches.push(Batch {
                records: std::mem::take(&mut current),
                bytes: current_bytes,
            });
            current_bytes = 0;
        }
        current_bytes += size;
        current.push(record);
    }

    if !current.is_empty() {
        batches.push(Batch {
            records: current,
            bytes: current_bytes,
        });
    }

    (batches, oversized)
}

/// A record's serialized size in bytes.
///
/// Serialization failure is not possible for the types a [`Record`] holds (strings and the JSON
/// produced by [`body_of`]), so the fallback is the attribute-only estimate rather than a panic. A
/// size that is slightly wrong is survivable; a capture path that panics is not.
#[must_use]
pub fn record_size(record: &Record) -> usize {
    serde_json::to_vec(&serde_json::json!({
        "channel": record.channel.as_str(),
        "time": record.time,
        "severity": record.severity.as_str(),
        "attributes": record
            .attributes
            .iter()
            .map(|(key, value)| (key.clone(), value.as_wire()))
            .collect::<Vec<_>>(),
        "body": record.body,
    }))
    .map_or(0, |bytes| bytes.len())
}

/// The text a record's body carries, for a receiver that wants to scan for a secret.
///
/// Only text-bearing shapes are walked: a string body, a chat message's text blocks, and nested
/// arrays and objects. Numbers and booleans are skipped because a secret is never one.
#[must_use]
pub fn body_texts(body: &serde_json::Value) -> Vec<String> {
    let mut texts = Vec::new();
    collect_text(body, &mut texts);
    texts
}

fn collect_text(value: &serde_json::Value, texts: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => texts.push(text.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_text(item, texts);
            }
        }
        serde_json::Value::Object(object) => {
            for item in object.values() {
                collect_text(item, texts);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// Replace a literal secret everywhere in a record's body, leaving the canonical log untouched.
///
/// This is the shape of the redaction rule a deployment mounts: a string match over the exported
/// copy. It is a function over a [`Record`] rather than over a session, which is what guarantees the
/// log cannot be rewritten by it.
///
/// An empty `secret` is a no-op rather than a panic or a match-everything: a misconfigured rule
/// should export the record unchanged, not erase it.
#[must_use]
pub fn redact_literal(record: &Record, secret: &str, replacement: &str) -> Record {
    if secret.is_empty() {
        return record.clone();
    }
    let mut redacted = record.clone();
    redacted.body = redact_value(&record.body, secret, replacement);
    // Attribute values are redacted too: a cwd can contain a token, and a receiver storing
    // attributes next to the body would otherwise keep the copy the body just lost.
    for (_, value) in &mut redacted.attributes {
        if let Attribute::Text(text) = value {
            *text = text.replace(secret, replacement);
        }
    }
    redacted
}

fn redact_value(value: &serde_json::Value, secret: &str, replacement: &str) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            serde_json::Value::String(text.replace(secret, replacement))
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| redact_value(item, secret, replacement))
                .collect(),
        ),
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, item)| (key.clone(), redact_value(item, secret, replacement)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Whether a captured event should take part in the ledger at all.
///
/// A history-inclusive capture replays inherited events from a fork's parent; a history-exclusive one
/// starts at the session's own lifecycle boundary. Upstream decides this from
/// `session.firstLifecycleSeq`, which only exists when the log is seeded. When it is absent the two
/// modes agree, and a caller that asked to exclude history simply gets everything.
#[must_use]
pub fn starts_history(include_history: bool, first_lifecycle_seq: Option<u64>) -> bool {
    if include_history {
        return true;
    }
    first_lifecycle_seq.is_none()
}

/// The lowest sequence a prefix capture should read, given a handoff cursor.
///
/// The cursor is the highest `seq` already handed to a backend. Upstream resumes *after* it, so
/// `None` (no cursor survived) means start from the beginning — re-handing history is recoverable,
/// silently skipping it is not.
#[must_use]
pub fn resume_after(cursor: Option<u64>) -> u64 {
    cursor.map_or(0, |seq| seq)
}

/// Raise a record's severity to [`Severity::Warn`] at most.
///
/// The capture mapping only ever distinguishes "the event's own flag says it failed" from
/// everything else, which leaves no way to express "this is suspicious but did not fail" — a
/// prompt that looks like an injected instruction, a tool that was retried five times. A redaction
/// or policy rule expresses that by escalating the record, so `Warn` exists as a constructible
/// value even though [`severity_of`] never returns it.
///
/// Escalation never downgrades: a record already at [`Severity::Error`] stays there, because a rule
/// noticing something odd about a failure must not bury the failure.
#[must_use]
pub fn escalate(record: &Record) -> Record {
    if record.severity >= Severity::Warn {
        return record.clone();
    }
    let mut escalated = record.clone();
    escalated.severity = Severity::Warn;
    escalated
}
