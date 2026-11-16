//! The structured half of session search: typed filters, semantic documents, snippets, and
//! request-bound cursors.
//!
//! # What upstream has, and what this crate already covers
//!
//! Upstream `packages/session-query` is 5,850 source lines across three packages plus a browser
//! client, measured as `src/*.ts` excluding tests. Grouped by capability family:
//!
//! | family | upstream files (lines) | local state |
//! |---|---|---|
//! | structured result filters and their validation | `filters.ts` (243), `types.ts` (290) | **nothing** |
//! | request normalization, predicate compile, snippet, fingerprint | `query.ts` (477) | **nothing** |
//! | semantic text extraction and searchable documents | `extraction.ts` (87), `documents.ts` (75) | `searchable_text` projects some message text; no placement, no documents |
//! | `SQLite` engine, schema, cursor codec | `index.ts` (1,131), `schema.ts` (173) | **nothing** |
//! | corpus resolution, observations, cold reads | `corpus.ts` (323), `observation.ts` (395), `cold-read.ts` (57), `sources.ts` (25) | `session.rs` reads a whole log; no live/persisted precedence |
//! | event and lineage tracing | `tracing.ts` (254) | `fold_surface` is the substrate; no trace |
//! | log export, Markdown, browser download controllers | `session-log-export` (1,828) | `session_export_rpc.rs` renders Markdown; no zip stream |
//! | query grammar (implicit AND, `-`, `OR`, phrases, parentheses) | **not upstream** | `query_syntax.rs` (442) |
//! | search over a catalogue | — | `session_query.rs` (416) |
//!
//! # The two biggest gaps, and why this module takes them
//!
//! **Filters.** [`girder_core::SearchQuery`] carries a kind, a session id, a text, and a limit.
//! That is the entire filter vocabulary: a caller who wants "sessions in this directory created last
//! week that contain this word, but only in tool results" cannot express it, and one who wants "any
//! of these three sessions" has to run three searches. Upstream answers all of it with five
//! logical-session clauses and five event clauses, each independently validated, `AND`ed across
//! clauses and `OR`ed within a clause's value list. None of it exists here, and all of it is decidable
//! without a database.
//!
//! **Semantic documents and snippets.** Local search matches against
//! [`girder_core::searchable_text`], which is a projection written for a different question ("what
//! text does this event hold"). It deliberately includes system and developer prompts and the
//! session title, drops tool-call blocks nested inside a message, and has no notion of an excerpt
//! around the match. Upstream's extraction is a *contract* about what is searchable, its documents
//! carry the surface placement that says whether an event is still in the model's context, and its
//! snippet is positioned on the match rather than cut from the front.
//!
//! # What is here
//!
//! - [`SessionFilter`] / [`EventFilter`] / [`MetadataFilter`]: typed clauses, plus
//!   [`EventPredicate`], the materialized form that has already compiled its text.
//! - [`filter_session_records`] and [`filter_event_documents`]: the AND-of-clauses,
//!   OR-of-values evaluation both upstream predicates implement.
//! - [`extract_session_event_text`], [`build_session_event_records`],
//!   [`build_session_event_search_documents`]: the searchable-text contract and the
//!   surface-aware documents built from it.
//! - [`SessionTextFilter`]: upstream's whitespace-flexible literal match.
//! - [`make_snippet`]: the bounded, match-centred excerpt, ported operation for operation.
//! - [`sanitize_fts_text`], [`quote_fts_data`]: reserved-marker removal and quote escaping.
//! - [`normalize_query`], [`normalize_limit`]: the two validations every search request needs.
//! - [`fingerprint_session_request`], [`fingerprint_event_request`]: the normalized identity a
//!   cursor is bound to.
//! - [`encode_search_cursor`], [`decode_search_cursor`]: an opaque token that is refused by any
//!   other request, scope, instance, or corpus generation.
//! - [`SessionQueryError`] and its closed [`SessionQueryErrorCode`] taxonomy.
//!
//! # What is deliberately not here
//!
//! - **No SQL.** Upstream's `buildSessionWhere`/`buildEventWhere` compile the same clauses into
//!   parameterized `SQLite` fragments. This crate has no `SQLite` dependency, and generating SQL
//!   strings no engine executes would be untestable in the only way that matters — a predicate that
//!   returns the wrong rows. The *semantics* those fragments encode are what
//!   [`filter_session_records`] and [`filter_event_documents`] implement, so an indexed backend can
//!   be added later without the meaning of a filter changing. The binding-count ceilings
//!   (`SQLITE_PORTABLE_VARIABLE_LIMIT` and the FTS5 outer-predicate budget) belong with that
//!   backend, because they are limits of a statement builder rather than of a filter.
//! - **No corpus.** Which session is live, which is persisted, and which header wins are I/O and
//!   service-lifetime questions ([`girder_core::SessionStore`] and the RPC layer answer them here).
//!   [`LogicalSession`] is the flat record a filter decides on, exactly like
//!   [`girder_core::FilterSummary`] in the list-filter path, and for the same reason: a filter must
//!   not force a log to be decoded.
//! - **No tracing.** [`girder_core::fold_surface`] already answers the surface question; the trace
//!   view is a separate piece of work.
//!
//! # Why the ranges validate at construction
//!
//! Upstream validates a range inside the predicate builder, which means an inverted range is a
//! runtime failure of the *search*, raised after the caller has already composed the request.
//! [`SessionQueryRange::new`] rejects a non-finite bound and an inverted interval instead, so a
//! [`SessionFilter`] that exists is a filter whose range can be evaluated. That is why
//! [`filter_session_records`] has no error case while upstream's `filterSessionResults` does: the
//! failure moved to the caller who wrote the bound.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// Imports go through the crate root's public re-exports rather than `crate::module` paths because
// this file is also compiled by `tests/session_query_gap.rs` through `#[path]`. In that build the
// file is a module of the *test* crate, where `crate::` names the test crate and every `crate::…`
// path fails to resolve. The root re-exports resolve identically in both builds, which is what
// keeps one source file the single thing under test.
use girder_core::{ContentBlock, SessionEvent, SurfaceEnvelope, TurnEndReason, fold_surface};

// ---------------------------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------------------------

/// The stable failure taxonomy shared by session reads, traces, and search.
///
/// Upstream `SessionQueryErrorCode`. The variants are one-to-one with upstream's members and the
/// strings in [`SessionQueryErrorCode::as_str`] are the contract with a transport: a host maps them
/// onto protocol errors, so they are spelled out rather than derived from variant names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionQueryErrorCode {
    /// The caller's signal cancelled the work.
    Aborted,
    /// A session log is not a valid log.
    CorruptSession,
    /// The requested event sequence does not exist in the session.
    EventNotFound,
    /// The search index backend failed.
    IndexFailed,
    /// The caller's configuration cannot be honoured.
    InvalidConfig,
    /// A continuation cursor is malformed or belongs to another request.
    InvalidCursor,
    /// A filter names an unknown value or an impossible range.
    InvalidFilter,
    /// A page size is outside the accepted interval.
    InvalidLimit,
    /// A query string is empty or holds a reserved character.
    InvalidQuery,
    /// Session parentage is ambiguous, cyclic, or inconsistent.
    InvalidLineage,
    /// A surface fold or a surface reference is invalid.
    InvalidSurface,
    /// A raw-event read window is out of bounds.
    InvalidWindow,
    /// The persistence backend failed.
    PersistenceFailed,
    /// Search is disabled in this deployment.
    SearchDisabled,
    /// No session has the requested id.
    SessionNotFound,
    /// A cursor names a corpus generation that has since changed.
    StaleCursor,
    /// Two observations of one session disagree about its immutable header.
    SourceConflict,
}

impl SessionQueryErrorCode {
    /// The wire spelling, as upstream writes it.
    ///
    /// Upstream `SessionQueryError['code']`: an upper-snake member of the closed taxonomy.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "SESSION_QUERY_ABORTED",
            Self::CorruptSession => "SESSION_QUERY_CORRUPT_SESSION",
            Self::EventNotFound => "SESSION_QUERY_EVENT_NOT_FOUND",
            Self::IndexFailed => "SESSION_QUERY_INDEX_FAILED",
            Self::InvalidConfig => "SESSION_QUERY_INVALID_CONFIG",
            Self::InvalidCursor => "SESSION_QUERY_INVALID_CURSOR",
            Self::InvalidFilter => "SESSION_QUERY_INVALID_FILTER",
            Self::InvalidLimit => "SESSION_QUERY_INVALID_LIMIT",
            Self::InvalidQuery => "SESSION_QUERY_INVALID_QUERY",
            Self::InvalidLineage => "SESSION_QUERY_INVALID_LINEAGE",
            Self::InvalidSurface => "SESSION_QUERY_INVALID_SURFACE",
            Self::InvalidWindow => "SESSION_QUERY_INVALID_WINDOW",
            Self::PersistenceFailed => "SESSION_QUERY_PERSISTENCE_FAILED",
            Self::SearchDisabled => "SESSION_QUERY_SEARCH_DISABLED",
            Self::SessionNotFound => "SESSION_QUERY_SESSION_NOT_FOUND",
            Self::StaleCursor => "SESSION_QUERY_STALE_CURSOR",
            Self::SourceConflict => "SESSION_QUERY_SOURCE_CONFLICT",
        }
    }

    /// Parses a wire spelling.
    ///
    /// Upstream's type has no parser because the value never arrives from outside; a Rust host that
    /// re-raises one of these over a protocol needs to read it back, so the inverse is explicit.
    /// Returns `None` for a spelling this build does not know rather than inventing a code.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "SESSION_QUERY_ABORTED" => Self::Aborted,
            "SESSION_QUERY_CORRUPT_SESSION" => Self::CorruptSession,
            "SESSION_QUERY_EVENT_NOT_FOUND" => Self::EventNotFound,
            "SESSION_QUERY_INDEX_FAILED" => Self::IndexFailed,
            "SESSION_QUERY_INVALID_CONFIG" => Self::InvalidConfig,
            "SESSION_QUERY_INVALID_CURSOR" => Self::InvalidCursor,
            "SESSION_QUERY_INVALID_FILTER" => Self::InvalidFilter,
            "SESSION_QUERY_INVALID_LIMIT" => Self::InvalidLimit,
            "SESSION_QUERY_INVALID_QUERY" => Self::InvalidQuery,
            "SESSION_QUERY_INVALID_LINEAGE" => Self::InvalidLineage,
            "SESSION_QUERY_INVALID_SURFACE" => Self::InvalidSurface,
            "SESSION_QUERY_INVALID_WINDOW" => Self::InvalidWindow,
            "SESSION_QUERY_PERSISTENCE_FAILED" => Self::PersistenceFailed,
            "SESSION_QUERY_SEARCH_DISABLED" => Self::SearchDisabled,
            "SESSION_QUERY_SESSION_NOT_FOUND" => Self::SessionNotFound,
            "SESSION_QUERY_STALE_CURSOR" => Self::StaleCursor,
            "SESSION_QUERY_SOURCE_CONFLICT" => Self::SourceConflict,
            _ => return None,
        })
    }
}

/// A typed session-query failure whose code is one closed taxonomy member.
///
/// Upstream `SessionQueryError`, which extends the harness error base with a narrowed `code`. The
/// detail string is carried for a message a caller can show; the code is what a caller branches on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionQueryError {
    code: SessionQueryErrorCode,
    detail: String,
}

impl SessionQueryError {
    /// Builds a failure with an explicit code.
    #[must_use]
    pub fn new(code: SessionQueryErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// The taxonomy member.
    #[must_use]
    pub fn code(&self) -> SessionQueryErrorCode {
        self.code
    }

    /// What went wrong, in a form a message can carry.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for SessionQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "session query ({}): {}",
            self.code.as_str(),
            self.detail
        )
    }
}

impl std::error::Error for SessionQueryError {}

/// Builds an [`SessionQueryErrorCode::InvalidFilter`] failure.
///
/// Upstream `invalidFilter`, which prefixes the detail with `session ` so the message reads as one
/// sentence at every call site.
fn invalid_filter(detail: impl Into<String>) -> SessionQueryError {
    SessionQueryError::new(
        SessionQueryErrorCode::InvalidFilter,
        format!("session {}", detail.into()),
    )
}

// ---------------------------------------------------------------------------------------------
// Ranges and closed value sets
// ---------------------------------------------------------------------------------------------

/// An inclusive numeric interval used by time, creation-time, and sequence filters.
///
/// Upstream `SessionResultRange`. Both bounds are optional, so an interval can be open on either
/// side or on both, and both are inclusive because a bound a person types ("since Tuesday", "at
/// least 5") reads that way in the language they typed it in.
///
/// The bounds are `f64` because upstream's are `JavaScript` numbers, and keeping that shape keeps
/// upstream's two refusals meaningful: a bound that is not finite (a division by zero upstream, a
/// parse of `"inf"` here) and an interval whose lower bound exceeds its upper one. Both are rejected
/// by [`SessionQueryRange::new`] rather than by the search that would have used the range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionQueryRange {
    from: Option<f64>,
    to: Option<f64>,
}

impl SessionQueryRange {
    /// Builds a validated interval.
    ///
    /// Upstream `validateRange`, moved to construction.
    ///
    /// # Errors
    ///
    /// Returns [`SessionQueryErrorCode::InvalidFilter`] when either bound is not finite, or when
    /// `from` exceeds `to`.
    pub fn new(from: Option<f64>, to: Option<f64>) -> Result<Self, SessionQueryError> {
        if from.is_some_and(|value| !value.is_finite()) {
            return Err(invalid_filter("range filter from must be finite"));
        }
        if to.is_some_and(|value| !value.is_finite()) {
            return Err(invalid_filter("range filter to must be finite"));
        }
        if let (Some(from), Some(to)) = (from, to)
            && from > to
        {
            return Err(invalid_filter(
                "range filter from must be less than or equal to to",
            ));
        }
        Ok(Self { from, to })
    }

    /// The interval that constrains nothing.
    ///
    /// Upstream expresses this as a filter object with neither bound, and `buildSessionWhere`
    /// compiles it to an empty predicate. Here the empty interval is a value so a caller does not
    /// have to spell `None` twice.
    #[must_use]
    pub fn unbounded() -> Self {
        Self {
            from: None,
            to: None,
        }
    }

    /// Builds an interval from two present bounds.
    ///
    /// # Errors
    ///
    /// As [`SessionQueryRange::new`].
    pub fn bounded(from: f64, to: f64) -> Result<Self, SessionQueryError> {
        Self::new(Some(from), Some(to))
    }

    /// The inclusive lower bound, when there is one.
    #[must_use]
    pub fn from(self) -> Option<f64> {
        self.from
    }

    /// The inclusive upper bound, when there is one.
    #[must_use]
    pub fn to(self) -> Option<f64> {
        self.to
    }

    /// Whether neither bound is present.
    #[must_use]
    pub fn is_unbounded(self) -> bool {
        self.from.is_none() && self.to.is_none()
    }

    /// Whether a value falls inside the interval.
    ///
    /// Upstream `matchesRange`, including its treatment of `NaN`: a comparison against `NaN` is
    /// false, so an unparseable value is inside every interval that has a bound on that side and
    /// outside none of them. An interval with no bound at all therefore accepts `NaN`, which is
    /// upstream's answer too — and the reason [`EventPredicate::Time`] refuses a document whose
    /// timestamp is *absent* rather than passing it in as `NaN`.
    #[must_use]
    pub fn contains(self, value: f64) -> bool {
        self.from.is_none_or(|from| value >= from) && self.to.is_none_or(|to| value <= to)
    }

    /// Whether a value is inside the interval *and* is a number.
    ///
    /// The predicate a filter on a recorded timestamp wants. [`SessionQueryRange::contains`] keeps
    /// upstream's raw comparison semantics, where an unbounded interval accepts `NaN` because there
    /// was nothing to compare against; a timestamp that arrived as `NaN` is not evidence of anything
    /// and must not match a filter just because the caller bounded only one end.
    #[must_use]
    pub fn accepts(self, value: f64) -> bool {
        value.is_finite() && self.contains(value)
    }

    /// Whether an event sequence falls inside the interval.
    ///
    /// The sequence is compared as a `f64` because that is what upstream's `seq` is: a `JavaScript`
    /// number cannot represent an integer above 2^53 exactly, so a bound and a sequence are
    /// compared in the same domain they were recorded in. A local log would have to hold a
    /// quadrillion events for the conversion to lose a bit.
    #[must_use]
    // A sequence above 2^53 is not representable in the JSON log format this crate reads, so the
    // conversion is exact for every value that can appear; the lint's general case cannot know that.
    #[allow(clippy::cast_precision_loss)]
    pub fn contains_seq(self, value: u64) -> bool {
        self.contains(value as f64)
    }
}

/// Whether a session currently exists in the live set, the persisted set, or both.
///
/// Upstream `SessionAvailability`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAvailability {
    /// The id currently exists in the running process.
    Live,
    /// The active persistence backend currently lists the id.
    Persisted,
}

impl SessionAvailability {
    /// The wire spelling.
    ///
    /// Upstream's `SessionAvailability` union members, verbatim.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Persisted => "persisted",
        }
    }

    /// Parses a wire spelling, refusing one this build does not know.
    ///
    /// Upstream `assertAllowedValues(filter.kind, values, ['live', 'persisted'])`: a value outside
    /// the set is a [`SessionQueryErrorCode::InvalidFilter`], never a silently ignored clause.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "live" => Some(Self::Live),
            "persisted" => Some(Self::Persisted),
            _ => None,
        }
    }
}

/// Whether an event is current model context, replaced context, or raw-log-only.
///
/// Upstream `SessionEventSurface`. The three states are what makes a search able to answer "is this
/// still in the conversation" — a compaction shadows earlier events without deleting them, so
/// "appears in the log" and "the model can see it" are different questions over the same bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEventSurface {
    /// The event is a node of the folded surface.
    Current,
    /// A later event replaced the range this event was in.
    Shadowed,
    /// The event never joined the surface: boundaries, accounting, and settings records.
    LogOnly,
}

impl SessionEventSurface {
    /// The wire spelling.
    ///
    /// Upstream's `SessionEventSurface` union members, verbatim.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Shadowed => "shadowed",
            Self::LogOnly => "log-only",
        }
    }

    /// Parses a wire spelling.
    ///
    /// Upstream `assertAllowedValues(..., ['current', 'shadowed', 'log-only'])`. An unknown value is
    /// [`SessionQueryErrorCode::InvalidFilter`] for the same reason as
    /// [`SessionAvailability::from_wire`].
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "current" => Some(Self::Current),
            "shadowed" => Some(Self::Shadowed),
            "log-only" => Some(Self::LogOnly),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Logical-session records and their filters
// ---------------------------------------------------------------------------------------------

/// The immutable facts about a session that a filter can decide on.
///
/// Upstream `SessionHeader`, reduced to the fields its filters read. A filter must not decode a log:
/// `createdAt`, `cwd`, and `parentSession` are fixed when the session is created, so they are
/// available from a listing that never opens a single event.
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalSessionHeader {
    /// The session id.
    pub id: String,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_at: f64,
    /// The working directory the session was created in, when it recorded one.
    pub cwd: Option<String>,
    /// The session it was forked from, when it was.
    pub parent_session: Option<String>,
}

/// One session a query may return, with its source availability.
///
/// Upstream `SessionRecord`. `live` and `persisted` are both carried because they are different
/// questions — a session can be listed by persistence and not be loaded, and a session can exist in
/// memory without having been materialized yet — and the `availability` filter asks about the
/// disjunction of them.
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalSession {
    /// The session's immutable facts.
    pub header: LogicalSessionHeader,
    /// Whether the id currently exists in the live set.
    pub live: bool,
    /// Whether persistence currently lists the id.
    pub persisted: bool,
}

/// One logical-session predicate.
///
/// Upstream `SessionResultFilter`. A filter array is `AND`ed; the `values` list within one clause is
/// `OR`ed. An empty value list therefore matches nothing (no value is present), which is different
/// from an absent clause (that clause constrains nothing) — the distinction upstream's `'0'` SQL
/// clause encodes, kept here as plain evaluation.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionFilter {
    /// Keep sessions whose id is in this list.
    Id(Vec<String>),
    /// Keep sessions whose working directory is in this list, `None` meaning "recorded no cwd".
    Cwd(Vec<Option<String>>),
    /// Keep sessions created inside this interval.
    CreatedAt(SessionQueryRange),
    /// Keep sessions forked from one of these ids, `None` meaning "forked from nothing".
    Parent(Vec<Option<String>>),
    /// Keep sessions whose availability is one of these.
    Availability(Vec<SessionAvailability>),
}

impl SessionFilter {
    /// Whether a record satisfies this clause.
    ///
    /// Upstream `sessionPredicate`. The range is already valid because
    /// [`SessionQueryRange::new`] built it, and the value sets are closed enums, so this cannot fail
    /// where upstream's sibling can throw: the two runtime checks it performed — a finite range and
    /// an allowed availability value — are unrepresentable here.
    #[must_use]
    pub fn accepts(&self, session: &LogicalSession) -> bool {
        match self {
            Self::Id(values) => values.iter().any(|value| value == &session.header.id),
            Self::Cwd(values) => values
                .iter()
                .any(|value| value.as_deref() == session.header.cwd.as_deref()),
            Self::CreatedAt(range) => range.accepts(session.header.created_at),
            Self::Parent(values) => values
                .iter()
                .any(|value| value.as_deref() == session.header.parent_session.as_deref()),
            Self::Availability(values) => values.iter().any(|value| match value {
                SessionAvailability::Live => session.live,
                SessionAvailability::Persisted => session.persisted,
            }),
        }
    }
}

/// Applies `AND`ed logical-session filters, preserving input order.
///
/// Upstream `filterSessionResults`. Returns borrows rather than clones: a filter decides, it does
/// not copy, and the caller already owns the listing.
///
/// The empty filter list matches every record, which is what makes "no filter" and "each clause in
/// the list holds" the same statement.
#[must_use]
pub fn filter_session_records<'a>(
    sessions: &'a [LogicalSession],
    filters: &[SessionFilter],
) -> Vec<&'a LogicalSession> {
    sessions
        .iter()
        .filter(|session| filters.iter().all(|filter| filter.accepts(session)))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Semantic documents and their filters
// ---------------------------------------------------------------------------------------------

/// Lightweight metadata for one event within a session.
///
/// Upstream `SessionEventRecord`. `time` is optional where upstream's is required because a local
/// event carries no timestamp at all — see [`build_session_event_records`] — and `session_id` is a
/// string because this crate's branded ids live in `girder_core::brand` and an event record is
/// read far more often than it is constructed.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionEventRecord {
    /// Session that owns the event.
    pub session_id: String,
    /// Zero-based position in the raw log.
    pub seq: u64,
    /// The event's `type` tag.
    pub type_name: String,
    /// Event timestamp in milliseconds, when the caller could supply one.
    pub time: Option<f64>,
    /// Where the event sits in the folded surface.
    pub surface: SessionEventSurface,
}

/// A searchable semantic document derived from one event.
///
/// Upstream `SessionEventSearchDocument`: the record plus the text a search actually scans. Keeping
/// the record inside the document rather than flattening it means a filter can read placement
/// without parsing the text, and a hit can be reported with the same metadata the filter saw.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionEventSearchDocument {
    /// The event's identity and placement.
    pub record: SessionEventRecord,
    /// The newline-joined semantic text.
    pub text: String,
}

impl SessionEventSearchDocument {
    /// The owning session id.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.record.session_id
    }

    /// The event's position in the raw log.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.record.seq
    }

    /// The event's `type` tag.
    #[must_use]
    pub fn type_name(&self) -> &str {
        &self.record.type_name
    }

    /// The event's timestamp, when known.
    #[must_use]
    pub fn time(&self) -> Option<f64> {
        self.record.time
    }

    /// Where the event sits in the folded surface.
    #[must_use]
    pub fn surface(&self) -> SessionEventSurface {
        self.record.surface
    }

    /// The semantic text a search scans.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// One event predicate as a caller writes it.
///
/// Upstream `SessionEventResultFilter`. The text clause holds the raw string, because compiling it
/// can fail and that failure belongs to [`materialize_event_filters`] rather than to the literal a
/// person typed into a filter box.
// The variants are a request vocabulary: a caller composes clauses and hands them to
// `materialize_event_filters`. This module never constructs one, so the crate-root build reports
// every variant as unconstructed even though the public API exists to be called.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub enum EventFilter {
    /// Keep events whose sequence is inside this interval.
    Seq(SessionQueryRange),
    /// Keep events whose timestamp is inside this interval.
    Time(SessionQueryRange),
    /// Keep events of these types.
    Type(Vec<String>),
    /// Keep events in these surface placements.
    Surface(Vec<SessionEventSurface>),
    /// Keep events whose semantic text matches this literal.
    Text(String),
}

/// One event predicate restricted to what a full-text backend can apply before ranking.
///
/// Upstream `SessionEventMetadataFilter`, which is `Exclude<SessionEventResultFilter, {kind:'text'}>`.
/// The exclusion is a distinct type here rather than a runtime check, so
/// `materializeMetadataFilters`'s refusal of a text clause — "session-search metadata filters do not
/// accept text clauses" — is a compile error instead.
#[derive(Clone, Debug, PartialEq)]
pub enum MetadataFilter {
    /// Keep events whose sequence is inside this interval.
    Seq(SessionQueryRange),
    /// Keep events whose timestamp is inside this interval.
    Time(SessionQueryRange),
    /// Keep events of these types.
    Type(Vec<String>),
    /// Keep events in these surface placements.
    Surface(Vec<SessionEventSurface>),
}

/// One event predicate in the form evaluation uses.
///
/// Upstream has no separate materialized type: `materializeSessionEventResultFilters` returns the
/// same shape it received and `eventPredicate` compiles the text clause on every call. Compiling
/// once and keeping the compiled form is the difference between a filter that can fail during
/// evaluation and one that cannot.
#[derive(Clone, Debug, PartialEq)]
pub enum EventPredicate {
    /// Keep events whose sequence is inside this interval.
    Seq(SessionQueryRange),
    /// Keep events whose timestamp is inside this interval.
    Time(SessionQueryRange),
    /// Keep events of these types.
    Type(Vec<String>),
    /// Keep events in these surface placements.
    Surface(Vec<SessionEventSurface>),
    /// Keep events whose semantic text matches this compiled literal.
    Text(SessionTextFilter),
}

impl EventPredicate {
    /// Whether a document satisfies this predicate.
    ///
    /// Upstream `eventPredicate`. A `time` bound on a document whose timestamp is unknown does not
    /// match: a bound that cannot be checked is not evidence that it holds.
    #[must_use]
    pub fn accepts(&self, document: &SessionEventSearchDocument) -> bool {
        match self {
            Self::Seq(range) => range.contains_seq(document.seq()),
            Self::Time(range) => document.time().is_some_and(|time| range.accepts(time)),
            Self::Type(values) => values.iter().any(|value| value == document.type_name()),
            Self::Surface(values) => values.contains(&document.surface()),
            Self::Text(filter) => filter.matches(document.text()),
        }
    }
}

/// Compiles and validates event clauses before an asynchronous boundary.
///
/// Upstream `materializeSessionEventResultFilters`. Upstream copies and, for `surface`, checks the
/// allowed values; the only failure left in this build is a text clause with no non-whitespace text,
/// which is compiled here so evaluation cannot fail later.
///
/// # Errors
///
/// Returns [`SessionQueryErrorCode::InvalidFilter`] when a `text` clause is empty or whitespace-only.
pub fn materialize_event_filters(
    filters: &[EventFilter],
) -> Result<Vec<EventPredicate>, SessionQueryError> {
    filters
        .iter()
        .map(|filter| {
            Ok(match filter {
                EventFilter::Seq(range) => EventPredicate::Seq(*range),
                EventFilter::Time(range) => EventPredicate::Time(*range),
                EventFilter::Type(values) => EventPredicate::Type(values.clone()),
                EventFilter::Surface(values) => EventPredicate::Surface(values.clone()),
                EventFilter::Text(text) => EventPredicate::Text(SessionTextFilter::compile(text)?),
            })
        })
        .collect()
}

/// Copies metadata clauses into their evaluable form.
///
/// Upstream `materializeMetadataFilters`, minus the text clause its input type allows and this one
/// does not. Infallible: every remaining clause is already-validated data.
#[must_use]
pub fn materialize_metadata_filters(filters: &[MetadataFilter]) -> Vec<EventPredicate> {
    filters
        .iter()
        .map(|filter| match filter {
            MetadataFilter::Seq(range) => EventPredicate::Seq(*range),
            MetadataFilter::Time(range) => EventPredicate::Time(*range),
            MetadataFilter::Type(values) => EventPredicate::Type(values.clone()),
            MetadataFilter::Surface(values) => EventPredicate::Surface(values.clone()),
        })
        .collect()
}

/// Applies `AND`ed event predicates to semantic documents, preserving input order.
///
/// Upstream `filterSessionEventDocuments`. The empty predicate list accepts every document, so the
/// two halves of "each clause holds" — no clause at all, and every clause that exists — are one
/// statement rather than a special case.
#[must_use]
pub fn filter_event_documents<'a>(
    documents: &'a [SessionEventSearchDocument],
    predicates: &[EventPredicate],
) -> Vec<&'a SessionEventSearchDocument> {
    documents
        .iter()
        .filter(|document| {
            predicates
                .iter()
                .all(|predicate| predicate.accepts(document))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Semantic text
// ---------------------------------------------------------------------------------------------

/// A compiled literal text filter: case-insensitive, whitespace-flexible, injection-free.
///
/// Upstream `compileSessionTextFilter`, which builds a regular expression from the caller's text
/// after escaping every regex metacharacter and joining the words with `\s+`. The point of compiling
/// is that the query is *data*: `a.*b` searches for the six characters `a.*b`, not for a pattern.
///
/// This build has no regular-expression dependency, and it does not need one: the constructed
/// pattern has exactly one shape — a sequence of literal terms separated by runs of whitespace — so
/// matching it is a scan, and a scan cannot be made to backtrack catastrophically by a crafted
/// query the way a general engine's can.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTextFilter {
    /// The literal terms, lowercased, in query order.
    terms: Vec<String>,
}

impl SessionTextFilter {
    /// Compiles a caller-provided literal.
    ///
    /// Upstream `compileSessionTextFilter`.
    ///
    /// # Errors
    ///
    /// Returns [`SessionQueryErrorCode::InvalidFilter`] when the text holds nothing but whitespace:
    /// an empty pattern would match every document, which is a filter that silently stops filtering.
    pub fn compile(text: &str) -> Result<Self, SessionQueryError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(invalid_filter(
                "text filter must contain non-whitespace text",
            ));
        }
        Ok(Self {
            terms: trimmed.split_whitespace().map(str::to_lowercase).collect(),
        })
    }

    /// The literal terms this filter matches, in query order.
    #[must_use]
    pub fn terms(&self) -> &[String] {
        &self.terms
    }

    /// Whether the text holds every term, in order, separated by whitespace.
    ///
    /// Upstream `pattern.test(document.text)`: an unanchored case-insensitive match. Case folding is
    /// done with [`str::to_lowercase`], which is Unicode-aware but is not the full case-folding
    /// upstream's `iu` flag performs — the two agree on every script whose lowercase is a
    /// character-for-character mapping, and differ only where a fold changes a string's length in a
    /// way a per-character comparison cannot align. That difference is accepted rather than hidden:
    /// the alternative is a Unicode case-folding table, which is a dependency this module does not
    /// have.
    #[must_use]
    pub fn matches(&self, text: &str) -> bool {
        let haystack = text.to_lowercase();
        match self.terms.first() {
            // `compile` refuses an empty query, so this is unreachable for a compiled filter; it is
            // written as a scan rather than an assertion because an empty term list genuinely
            // constrains nothing.
            None => true,
            Some(_) => self.matches_from(&haystack, 0, 0),
        }
    }

    /// Whether every term from `index` onward appears in order at or after byte offset `from`.
    ///
    /// Every occurrence of the current term is tried, not just the first: `a b` must match
    /// `a xa b` at the second `a`, which a single greedy pass would miss. The recursion is bounded
    /// by the number of terms, which is bounded by the query length.
    fn matches_from(&self, haystack: &str, index: usize, from: usize) -> bool {
        let Some(term) = self.terms.get(index) else {
            return true;
        };
        let Some(rest) = haystack.get(from..) else {
            return false;
        };
        for (offset, _) in rest.match_indices(term.as_str()) {
            let after = from + offset + term.len();
            if index + 1 == self.terms.len() {
                return true;
            }
            // The separator is one or more whitespace characters. Consuming the whole run is
            // equivalent to the pattern's greedy `\s+` followed by a term that cannot itself begin
            // with whitespace, so no shorter separator can succeed where this one fails.
            let Some(tail) = haystack.get(after..) else {
                return false;
            };
            let separator = tail.len() - tail.trim_start().len();
            if separator == 0 {
                continue;
            }
            if self.matches_from(haystack, index + 1, after + separator) {
                return true;
            }
        }
        false
    }
}

/// The marker a full-text backend inserts before a matching run.
///
/// Upstream `FTS_HIGHLIGHT_START`. U+FDD0 is a Unicode noncharacter: no real document contains it,
/// so a marker collision can only come from a corrupt or hostile log — which is why
/// [`sanitize_fts_text`] removes it from text on the way in.
pub const FTS_HIGHLIGHT_START: char = '\u{FDD0}';

/// The marker a full-text backend inserts after a matching run.
///
/// Upstream `FTS_HIGHLIGHT_END`, U+FDD1.
pub const FTS_HIGHLIGHT_END: char = '\u{FDD1}';

/// Removes characters that would collide with the highlight markers or the terminator.
///
/// Upstream `sanitizeFtsText`. Applied both to extracted document text and to a caller's query,
/// because the markers are how [`make_snippet`] locates the match: a document that fabricated one
/// could make a snippet claim a match position that never existed.
#[must_use]
pub fn sanitize_fts_text(text: &str) -> String {
    text.replace(['\0', FTS_HIGHLIGHT_START, FTS_HIGHLIGHT_END], "\u{FFFD}")
}

/// Quotes caller text as one phrase so query syntax stays inert data.
///
/// Upstream `quoteFtsData`. Doubling every embedded quote is what keeps `foo" OR bar` from becoming
/// an operator: inside an escaped phrase there is no syntax left to express.
#[must_use]
pub fn quote_fts_data(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// Builds a whitespace-normalized excerpt of at most `max_chars` characters, positioned on the match.
///
/// Upstream `makeSnippet`, ported operation for operation. Three properties are load-bearing and
/// none of them is cosmetic:
///
/// - **The bound is in characters, not bytes.** A snippet cut at a byte offset would split a
///   multi-byte character, and the replacement character that produces reads as a corrupt log rather
///   than as a shortened excerpt.
/// - **The window follows the match.** A prefix excerpt of a long tool result shows the command and
///   not the answer, which is the opposite of what a search result is for. The match is placed about
///   a third of the way in, leaving the reader the context that led to it.
/// - **The ellipses are the only signal that the excerpt was cut.** A caller that renders a snippet
///   with no ellipsis is claiming the document said exactly that; one that renders it with an
///   ellipsis is telling the truth about what it omitted.
///
/// A `max_chars` of zero yields an empty string: nothing can be shown in no space, and returning a
/// one-character excerpt would break the bound the caller set.
#[must_use]
pub fn make_snippet(marked_text: &str, max_chars: usize) -> String {
    let (clean, match_start) = normalize_marked_text(marked_text);
    let characters: Vec<char> = clean.chars().collect();
    let Ok(length) = i64::try_from(characters.len()) else {
        // A document longer than i64::MAX characters cannot be held in memory on any target this
        // crate builds for; returning the normalized text keeps the function total.
        return clean;
    };
    let Ok(max) = i64::try_from(max_chars) else {
        return clean;
    };
    if length <= max {
        return clean;
    }
    if max <= 0 {
        return String::new();
    }
    if max == 1 {
        return "…".to_owned();
    }

    let matched = i64::try_from(match_start)
        .unwrap_or(i64::MAX)
        .min(length - 1);
    let mut start = (matched - max / 3).max(0);
    // The prefix ellipsis is decided once, from the initial window. Recomputing it after the window
    // moves to the end would drop the mark on exactly the case it exists for.
    let prefix: &str = if start > 0 { "…" } else { "" };
    // Each ellipsis is one character, so its width in the budget is one bit rather than a count.
    let prefix_width: i64 = i64::from(start > 0);
    let mut suffix: &str = "…";
    let mut content = max - prefix_width - 1;
    if content < 1 {
        start = matched;
        suffix = "";
        content = max - prefix_width;
    } else if matched >= start + content {
        start = matched - content + 1;
    }
    let mut end = (start + content).min(length);
    if end == length {
        suffix = "";
        content = max - prefix_width;
        start = (end - content).max(0);
    }
    end = (start + content).min(length);

    let begin = usize::try_from(start).unwrap_or(0);
    let stop = usize::try_from(end).unwrap_or(characters.len());
    let body: String = characters
        .get(begin..stop)
        .map_or_else(String::new, |window| window.iter().collect());
    format!("{prefix}{body}{suffix}")
}

/// Strips the highlight markers and collapses whitespace, reporting where the match began.
///
/// Upstream `normalizeMarkedText`. Whitespace runs become one space, leading whitespace is dropped,
/// and a trailing space is removed, so a snippet reads as a line of prose rather than as the
/// document's raw layout. `match_start` is a character index into the returned text, or zero when
/// the document carried no marker.
fn normalize_marked_text(marked_text: &str) -> (String, usize) {
    let mut characters: Vec<char> = Vec::new();
    let mut match_start: Option<usize> = None;
    for character in marked_text.chars() {
        if character == FTS_HIGHLIGHT_START {
            if match_start.is_none() {
                match_start = Some(characters.len());
            }
            continue;
        }
        if character == FTS_HIGHLIGHT_END {
            continue;
        }
        if character.is_whitespace() {
            if characters.last().is_some_and(|last| *last != ' ') {
                characters.push(' ');
            }
        } else {
            characters.push(character);
        }
    }
    if characters.last() == Some(&' ') {
        characters.pop();
    }
    (characters.into_iter().collect(), match_start.unwrap_or(0))
}

// ---------------------------------------------------------------------------------------------
// Semantic text extraction and document projection
// ---------------------------------------------------------------------------------------------

/// Extracts the semantic text a search should find for one event.
///
/// Upstream `extractSessionEventText`. This is a *contract*, not a dump of the payload:
///
/// | event | semantic text |
/// |---|---|
/// | `user/message`, `assistant/message` | each text and tool-call block, newline-joined |
/// | `tool/call` | the tool name and its raw arguments |
/// | `tool/result` | its text content |
/// | `turn/end` | the outcome, and the failure message when the turn failed |
/// | boundaries, accounting, settings, prompts | nothing |
///
/// Reasoning blocks contribute nothing. They are the model's private scratchpad, they are not shown
/// to a reader, and making them searchable would surface a phrase in a result that the conversation
/// never said aloud.
///
/// `system/message` and `developer/message` also contribute nothing, which is upstream's rule and
/// is deliberately *not* what [`girder_core::searchable_text`] does. That function answers a
/// different question — "what text does this event hold" — and changing it would silently move the
/// results of the search tool that already ships. The two are kept side by side, with tests that
/// assert both behaviours, so the difference is a documented decision rather than a drift.
#[must_use]
pub fn extract_session_event_text(event: &SessionEvent) -> String {
    match event {
        SessionEvent::UserMessage { message } | SessionEvent::AssistantMessage { message } => {
            content_text(&message.content)
        }
        SessionEvent::ToolCall {
            name, arguments, ..
        } => join_text([name.as_str(), arguments.as_str()]),
        // Upstream also appends the failure's `name` and `code`. A local tool result records only
        // `is_error`, so there is no failure identity to add; the content is the whole text.
        SessionEvent::ToolResult { content, .. } => content_text(content),
        SessionEvent::TurnEnd { reason, .. } => turn_end_text(reason),
        // Boundaries, accounting, settings, prompts, and titles: nothing searchable. Upstream's
        // `default` arm covers the same set, and its comment says why — an event that happens to
        // carry strings does not become searchable until its owner defines what it means.
        _ => String::new(),
    }
}

/// The semantic text of a turn outcome.
///
/// Upstream `turnEndText`. A completed turn contributes nothing: "the model finished" is the normal
/// case and making it searchable would put every turn in the results of a search for `completed`.
/// A failure keeps the word `error` so it is findable, and the message so the hit is useful.
fn turn_end_text(reason: &TurnEndReason) -> String {
    match reason {
        TurnEndReason::Completed => String::new(),
        TurnEndReason::Aborted { .. } => "aborted".to_owned(),
        TurnEndReason::Blocked => "blocked".to_owned(),
        TurnEndReason::Budget => "budget".to_owned(),
        TurnEndReason::Failed { message, .. } => join_text(["error", message.as_str()]),
    }
}

/// The semantic text of one message's content blocks.
///
/// Upstream `contentText`. A tool-call block nested in an assistant message is searchable by name and
/// arguments, because that is where a model's request for a tool lives when a provider streams it as
/// content rather than as a separate event.
fn content_text(content: &[ContentBlock]) -> String {
    let parts: Vec<&str> = content
        .iter()
        .flat_map(|block| match block {
            ContentBlock::Text { text } => vec![text.as_str()],
            ContentBlock::ToolCall {
                name, arguments, ..
            } => vec![name.as_str(), arguments.as_str()],
            // Reasoning is private; an attachment reference is a handle, not content.
            ContentBlock::Reasoning { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::File { .. } => Vec::new(),
        })
        .collect();
    join_text(parts)
}

/// Joins the non-blank parts with newlines.
///
/// Upstream `joinText`: each part is trimmed and blank parts are dropped, so a message that holds
/// one empty text block does not produce a document consisting of a newline.
fn join_text<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts
        .into_iter()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Classifies every position of a log as current, shadowed, or log-only.
///
/// Upstream `classifySurface`, expressed over what [`girder_core::fold_surface`] already returns.
/// The fold's node list is the current surface; each replacement's shadowed list is the set a later
/// event covered. Anything in neither is log-only.
///
/// # Errors
///
/// Returns [`SessionQueryErrorCode::InvalidSurface`] when the log cannot be folded — upstream
/// `SESSION_QUERY_INVALID_SURFACE` around the same call — because a document set built from a log
/// whose placement is unknown would mislabel every event after the corruption.
pub fn classify_surfaces(
    envelopes: &[SurfaceEnvelope],
) -> Result<Vec<SessionEventSurface>, SessionQueryError> {
    let folded = fold_surface(envelopes).map_err(|error| {
        SessionQueryError::new(
            SessionQueryErrorCode::InvalidSurface,
            format!("invalid session surface: {error}"),
        )
    })?;
    Ok(envelopes
        .iter()
        .map(|envelope| {
            if folded.is_visible(envelope.seq) {
                SessionEventSurface::Current
            } else if folded
                .replacements()
                .iter()
                .any(|replacement| replacement.shadowed.contains(&envelope.seq))
            {
                SessionEventSurface::Shadowed
            } else {
                SessionEventSurface::LogOnly
            }
        })
        .collect())
}

/// Projects a raw log into lightweight surface-aware event records.
///
/// Upstream `buildSessionEventRecords`.
///
/// The `time_of` closure is where this port has to differ from upstream. A local
/// [`SessionEvent`] carries no timestamp — upstream records one per event — so there is nothing in
/// the envelope to read. Rather than fabricate a time (or default to zero, which would put every
/// event inside every `from`-bounded filter), the caller supplies the mapping it can defend: the
/// log's modification time, a timestamp from a sidecar index, or `|_| None` when there is none.
///
/// # Errors
///
/// As [`classify_surfaces`].
pub fn build_session_event_records(
    session_id: &str,
    envelopes: &[SurfaceEnvelope],
    time_of: impl Fn(u64) -> Option<f64>,
) -> Result<Vec<SessionEventRecord>, SessionQueryError> {
    let surfaces = classify_surfaces(envelopes)?;
    Ok(envelopes
        .iter()
        .zip(surfaces)
        .map(|(envelope, surface)| SessionEventRecord {
            session_id: session_id.to_owned(),
            seq: envelope.seq,
            type_name: envelope.type_name().to_owned(),
            time: time_of(envelope.seq),
            surface,
        })
        .collect())
}

/// Builds the searchable documents of one complete raw event log.
///
/// Upstream `buildSessionEventSearchDocuments`. Events whose semantic text is empty are omitted
/// rather than indexed with empty text: an empty document matches no positive term but *does* match
/// an empty query, and a search that returns boundary events for an empty query is a search whose
/// results say nothing.
///
/// # Errors
///
/// As [`classify_surfaces`].
pub fn build_session_event_search_documents(
    session_id: &str,
    envelopes: &[SurfaceEnvelope],
    time_of: impl Fn(u64) -> Option<f64>,
) -> Result<Vec<SessionEventSearchDocument>, SessionQueryError> {
    let surfaces = classify_surfaces(envelopes)?;
    Ok(envelopes
        .iter()
        .zip(surfaces)
        .filter_map(|(envelope, surface)| {
            let text = extract_session_event_text(&envelope.event);
            if text.is_empty() {
                return None;
            }
            Some(SessionEventSearchDocument {
                record: SessionEventRecord {
                    session_id: session_id.to_owned(),
                    seq: envelope.seq,
                    type_name: envelope.type_name().to_owned(),
                    time: time_of(envelope.seq),
                    surface,
                },
                text,
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Request normalization
// ---------------------------------------------------------------------------------------------

/// The largest page size whose internal lookahead remains an exact `SQLite` integer binding.
///
/// Upstream `SQLITE_MAX_PAGE_LIMIT`, which is `Number.MAX_SAFE_INTEGER - 1`. The value is a
/// `JavaScript` limit rather than a `SQLite` one, and keeping it means a limit accepted here is
/// inside the interval upstream's engine accepts — a client cannot learn that its page size is fine
/// on one host and refused on another.
pub const SQLITE_MAX_PAGE_LIMIT: u64 = 9_007_199_254_740_991 - 1;

/// Limit defaults a search request is normalized against.
///
/// Upstream `QueryLimits`. Two numbers rather than one because "the caller did not say" and "the
/// caller may not ask for more than this" are different questions, and a single default conflates
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryLimits {
    /// Page size used when the request omits one.
    pub default_limit: u64,
    /// Largest accepted page size.
    pub max_limit: u64,
}

/// Validates and canonicalizes a search query string.
///
/// Upstream `normalizeQuery`: trim, collapse internal whitespace to one space, refuse an empty
/// query, refuse NUL, then remove the reserved highlight markers.
///
/// Each refusal is a different caller mistake. An empty query is a request with nothing to search
/// for; NUL cannot survive as text and would be stored as a terminator by a C-backed index; and a
/// reserved marker would let a query forge the marker [`make_snippet`] uses to place its window.
///
/// # Errors
///
/// Returns [`SessionQueryErrorCode::InvalidQuery`] for an empty or NUL-bearing query.
pub fn normalize_query(value: &str) -> Result<String, SessionQueryError> {
    let collapsed = value.split_whitespace().collect::<Vec<&str>>().join(" ");
    if collapsed.is_empty() {
        return Err(SessionQueryError::new(
            SessionQueryErrorCode::InvalidQuery,
            "session-search query must contain non-whitespace text",
        ));
    }
    if collapsed.contains('\0') {
        return Err(SessionQueryError::new(
            SessionQueryErrorCode::InvalidQuery,
            "session-search query must not contain NUL",
        ));
    }
    Ok(sanitize_fts_text(&collapsed))
}

/// Applies the configured page-size bounds to a request's limit.
///
/// Upstream `normalizeLimit`. An absent limit takes the default; a present one must be at least one
/// and at most the smaller of the configured maximum and [`SQLITE_MAX_PAGE_LIMIT`].
///
/// # Errors
///
/// Returns [`SessionQueryErrorCode::InvalidLimit`] when the limit is zero or above the maximum.
pub fn normalize_limit(limit: Option<u64>, limits: QueryLimits) -> Result<u64, SessionQueryError> {
    let limit = limit.unwrap_or(limits.default_limit);
    let maximum = limits.max_limit.min(SQLITE_MAX_PAGE_LIMIT);
    if limit < 1 || limit > maximum {
        return Err(SessionQueryError::new(
            SessionQueryErrorCode::InvalidLimit,
            format!("session-search limit must be an integer between 1 and {maximum}"),
        ));
    }
    Ok(limit)
}

// ---------------------------------------------------------------------------------------------
// Request fingerprints
// ---------------------------------------------------------------------------------------------

/// Builds the stable identity of a normalized cross-session request.
///
/// Upstream `requestFingerprint` for the `sessions` scope: the query, both filter lists in
/// canonical order, and the limit.
///
/// Canonicalization is what makes the identity useful. Value lists are sorted and de-duplicated
/// because a clause's values are `OR`ed, so `['a','b']` and `['b','a','b']` are the same
/// question; clauses are sorted by their serialized form because the clause list is `AND`ed, so the order a
/// caller wrote them in carries no meaning. Two requests that differ in neither therefore produce
/// the same string, which is what lets a cursor be refused by any request that *would* have returned
/// different rows.
///
/// The exact spelling is this build's own: upstream emits `JavaScript`'s `JSON.stringify`, and a
/// local copy of that byte sequence would be a fidelity claim this module cannot test. What is
/// preserved is the equivalence — same question, same string; different question, different string.
#[must_use]
pub fn fingerprint_session_request(
    query: &str,
    session_filters: &[SessionFilter],
    event_filters: &[MetadataFilter],
    limit: u64,
) -> String {
    let mut sessions: Vec<String> = session_filters
        .iter()
        .map(|filter| canonical_session_filter(filter).to_string())
        .collect();
    sessions.sort();
    let mut events: Vec<String> = event_filters
        .iter()
        .map(|filter| canonical_metadata_filter(filter).to_string())
        .collect();
    events.sort();
    json!({
        "scope": "sessions",
        "query": query,
        "sessionFilters": sessions,
        "eventFilters": events,
        "limit": limit,
    })
    .to_string()
}

/// Builds the stable identity of a normalized within-session request.
///
/// Upstream `requestFingerprint` for the `events` scope. The session id is part of the identity
/// because the same query over another session is another question — a cursor from one session must
/// not page another one's results.
#[must_use]
pub fn fingerprint_event_request(
    session_id: &str,
    query: &str,
    filters: &[MetadataFilter],
    limit: u64,
) -> String {
    let mut events: Vec<String> = filters
        .iter()
        .map(|filter| canonical_metadata_filter(filter).to_string())
        .collect();
    events.sort();
    json!({
        "scope": "events",
        "sessionId": session_id,
        "query": query,
        "filters": events,
        "limit": limit,
    })
    .to_string()
}

/// The canonical form of one logical-session clause.
fn canonical_session_filter(filter: &SessionFilter) -> Value {
    match filter {
        SessionFilter::Id(values) => {
            let mut values = values.clone();
            values.sort();
            values.dedup();
            json!({ "kind": "id", "values": values })
        }
        SessionFilter::Cwd(values) => {
            let mut values = values.clone();
            // `Option<String>` orders `None` before `Some`, which is upstream's `compareNullable`:
            // "recorded no working directory" sorts before any directory that was recorded.
            values.sort();
            values.dedup();
            json!({ "kind": "cwd", "values": values })
        }
        SessionFilter::CreatedAt(range) => json!({
            "kind": "created-at",
            "from": range.from(),
            "to": range.to(),
        }),
        SessionFilter::Parent(values) => {
            let mut values = values.clone();
            values.sort();
            values.dedup();
            json!({ "kind": "parent", "values": values })
        }
        SessionFilter::Availability(values) => {
            let mut values: Vec<&str> = values.iter().map(|value| value.as_str()).collect();
            values.sort_unstable();
            values.dedup();
            json!({ "kind": "availability", "values": values })
        }
    }
}

/// The canonical form of one event-metadata clause.
fn canonical_metadata_filter(filter: &MetadataFilter) -> Value {
    match filter {
        MetadataFilter::Seq(range) => json!({
            "kind": "seq",
            "from": range.from(),
            "to": range.to(),
        }),
        MetadataFilter::Time(range) => json!({
            "kind": "time",
            "from": range.from(),
            "to": range.to(),
        }),
        MetadataFilter::Type(values) => {
            let mut values = values.clone();
            values.sort();
            values.dedup();
            json!({ "kind": "type", "values": values })
        }
        MetadataFilter::Surface(values) => {
            let mut values: Vec<&str> = values.iter().map(|value| value.as_str()).collect();
            values.sort_unstable();
            values.dedup();
            json!({ "kind": "surface", "values": values })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pagination cursors
// ---------------------------------------------------------------------------------------------

/// Which of the two search scopes a cursor was issued for.
///
/// Upstream's `CursorPayload['scope']`. Present so a within-session cursor cannot be handed to the
/// cross-session method: the two page over different row sets, and an offset that is meaningful in
/// one names unrelated rows in the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorScope {
    /// Cross-session search.
    Sessions,
    /// Within-session event search.
    Events,
}

impl CursorScope {
    /// The wire spelling.
    ///
    /// Upstream's `CursorPayload['scope']` values. Exposed because a caller building a
    /// [`SearchCursorBinding`] by hand — a resumed search reading its state back from disk — needs
    /// to name the scope without relying on the `serde` representation staying the same.
    #[must_use]
    // Reached through the `serde` representation and through callers that spell a scope out; the
    // crate-root build has no such caller, so it reports the method as unused.
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sessions => "sessions",
            Self::Events => "events",
        }
    }
}

/// What a cursor is bound to: the search instance, the scope, the request, and the corpus.
///
/// Upstream passes these four values as separate arguments to `decodeCursor`; grouping them means a
/// caller cannot pass the fingerprint where the generation was meant, which is the one mistake that
/// would turn a stale-cursor refusal into a silent mis-pagination.
///
/// The `generation` is the only field that is allowed to *change* while a search is paged, and it is
/// checked last and reported separately: a cursor whose request still matches but whose corpus moved
/// is [`SessionQueryErrorCode::StaleCursor`], not [`SessionQueryErrorCode::InvalidCursor`], because
/// the caller's remedy is to restart the search rather than to fix their request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchCursorBinding {
    /// Identity of the search instance that issued the cursor.
    pub instance: String,
    /// The scope the cursor pages over.
    pub scope: CursorScope,
    /// The fingerprint of the normalized request.
    pub fingerprint: String,
    /// Identity of the corpus generation the page was cut from.
    pub generation: String,
}

/// The decoded contents of a cursor.
///
/// Upstream `CursorPayload`. Private to the codec: a caller receives an offset and hands a token
/// back, so the fields never need to leave this module — the encoding is what the opacity protects.
#[derive(Serialize, Deserialize)]
struct CursorPayload {
    version: u8,
    instance: String,
    scope: CursorScope,
    fingerprint: String,
    generation: String,
    offset: u64,
}

/// The only cursor encoding this build writes.
///
/// Upstream `decoded.version !== 1`. A cursor from another version is not migrated: its offset was
/// computed over rows this build may not order the same way, so it is refused and the caller starts
/// again.
const CURSOR_VERSION: u8 = 1;

/// Encodes an opaque continuation token.
///
/// Upstream `encodeCursor`: the payload as UTF-8 `JSON`, then base64url without padding. The result
/// is URL-safe and shell-safe, which is what lets it travel as a query parameter or a tool argument
/// without a second layer of escaping.
///
/// # Errors
///
/// Returns [`SessionQueryErrorCode::InvalidCursor`] when the payload cannot be serialized. The
/// payload is a fixed struct of strings and an integer, so this is unreachable in practice; it is a
/// `Result` rather than a panic because the alternative in this codebase is a forbidden `unwrap`.
pub fn encode_search_cursor(
    offset: u64,
    binding: &SearchCursorBinding,
) -> Result<String, SessionQueryError> {
    let payload = CursorPayload {
        version: CURSOR_VERSION,
        instance: binding.instance.clone(),
        scope: binding.scope,
        fingerprint: binding.fingerprint.clone(),
        generation: binding.generation.clone(),
        offset,
    };
    let bytes = serde_json::to_vec(&payload).map_err(|error| {
        SessionQueryError::new(
            SessionQueryErrorCode::InvalidCursor,
            format!("session-search cursor cannot be encoded: {error}"),
        )
    })?;
    Ok(base64url_encode(&bytes))
}

/// Decodes a continuation token, refusing one that does not belong to this request.
///
/// Upstream `decodeCursor`, including the order of its checks: a token that is not a token at all, a
/// token with the wrong version, instance, scope, or fingerprint, and a token whose offset is not a
/// non-negative integer are all [`SessionQueryErrorCode::InvalidCursor`]; only a token whose
/// `generation` moved is [`SessionQueryErrorCode::StaleCursor`].
///
/// # Errors
///
/// Returns [`SessionQueryErrorCode::InvalidCursor`] or [`SessionQueryErrorCode::StaleCursor`] as
/// above.
pub fn decode_search_cursor(
    cursor: &str,
    binding: &SearchCursorBinding,
) -> Result<u64, SessionQueryError> {
    let invalid = |detail: &str| {
        SessionQueryError::new(
            SessionQueryErrorCode::InvalidCursor,
            format!("session-search cursor is invalid: {detail}"),
        )
    };
    let bytes = base64url_decode(cursor).ok_or_else(|| invalid("not base64url text"))?;
    let text = String::from_utf8(bytes).map_err(|_| invalid("not UTF-8"))?;
    let payload: CursorPayload =
        serde_json::from_str(&text).map_err(|_| invalid("not a cursor payload"))?;
    if payload.version != CURSOR_VERSION
        || payload.instance != binding.instance
        || payload.scope != binding.scope
        || payload.fingerprint != binding.fingerprint
    {
        return Err(invalid("cursor does not belong to this normalized request"));
    }
    if payload.generation != binding.generation {
        return Err(SessionQueryError::new(
            SessionQueryErrorCode::StaleCursor,
            "session-search cursor is stale because its relevant corpus changed",
        ));
    }
    Ok(payload.offset)
}

/// The base64url alphabet, RFC 4648 §5, without padding.
const BASE64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encodes bytes as unpadded base64url.
fn base64url_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk.first().copied().unwrap_or(0));
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        let packed = (first << 16) | (second << 8) | third;
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            // A two-byte tail carries two characters, a one-byte tail one; the rest are padding
            // that base64url omits.
            if index > chunk.len() {
                break;
            }
            let sextet = usize::try_from((packed >> shift) & 0x3f).unwrap_or(0);
            if let Some(character) = BASE64URL_ALPHABET.get(sextet) {
                encoded.push(char::from(*character));
            }
        }
    }
    encoded
}

/// Decodes unpadded base64url, refusing a character outside the alphabet.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut sextets: Vec<u32> = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        let sextet = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        sextets.push(u32::from(sextet));
    }
    // A residue of one sextet encodes no whole byte; nothing this encoder wrote has that length.
    if sextets.len() % 4 == 1 {
        return None;
    }
    let mut decoded = Vec::with_capacity(sextets.len() / 4 * 3 + 2);
    for chunk in sextets.chunks(4) {
        let mut packed: u32 = 0;
        for (index, sextet) in chunk.iter().enumerate() {
            packed |= sextet << (18 - 6 * index);
        }
        decoded.push(u8::try_from((packed >> 16) & 0xff).unwrap_or(0));
        if chunk.len() > 2 {
            decoded.push(u8::try_from((packed >> 8) & 0xff).unwrap_or(0));
        }
        if chunk.len() > 3 {
            decoded.push(u8::try_from(packed & 0xff).unwrap_or(0));
        }
    }
    Some(decoded)
}
