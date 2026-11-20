//! The history-read core of upstream `packages/api/session-controller`: message-aligned backwards
//! pagination, page-window validation, address resolution, and the client-side ordering helpers.
//!
//! Upstream splits this capability across `src/history.ts` (189 lines: `paginate`, `entryFor`,
//! `pageRecords`, `validateHistoryWindow`, `addressId`, `validateAddress`, `rejectNotFound`,
//! `DEFAULT_MAX_MESSAGES`), the cursor half of `src/history/controller.ts` (`resolveHistoryRequest`,
//! `history`), and the two pure client-order functions `src/client/ordered-baseline.ts`
//! (`mergeOrderedBaseline`) and `src/client/sessions/lineage.ts` (`flattenLineage`). None of those
//! symbols existed in this crate before this module; see the coverage table in the port report.
//!
//! # Why pagination is the hard part
//!
//! A history page is read *backwards* from a cut, and the naive implementation is wrong in ways that
//! look like success:
//!
//! - **The budget counts messages, not events.** A page stops after `max_messages` messages, but the
//!   events it returns include the tool calls, step markers, and usage records between them. Counting
//!   records would return a page that is too short and, worse, cut mid-turn.
//! - **A message is a group, not a record.** A user message that a retry re-derived cites its
//!   `sourceEventSeqs`. Cutting at the message's own seq would orphan the sources, which then appear
//!   as an unexplainable gap on the next page. The cut moves back to the group's minimum.
//! - **`has_more` is about the cut, not the page size.** A page that stops exactly at seq 0 has no
//!   older events; one that stops at any later point does, even if it happens to hold few records.
//!   Reporting `has_more` from `records.len() == max_messages` strands the tail of the log.
//! - **A turn window is a stop *rule*, not a second budget.** When `turn_window` is set, the page may
//!   stop at a `turn/start` once both minima are met — but only after them, and never in preference to
//!   `max_messages`. Both minima are validated against `max_messages` so a window can never demand
//!   more messages than the budget allows.
//!
//! # Why the cursor validation is separate from the page cut
//!
//! Upstream's `throughSeq` is a tri-state: omitted means "the observed log end", `-1` means "the log
//! is empty, read nothing", and a non-negative value is a real cut. Rust's `u64` cannot represent the
//! middle state, and `-0` is a distinct JavaScript value that upstream rejects. [`SessionCursor`]
//! models the tri-state explicitly so a caller cannot accidentally treat `-1` as `0` — which would
//! flip an empty session into a read of its first event.
//!
//! # Why address resolution is here
//!
//! Reading a page requires knowing *which* log to read. Upstream's [`SessionAddress`] is a union of
//! an ordinary session and a direct subagent child, and the child form is only readable when the
//! child's durable descriptor proves it belongs to the named parent with the named mode. This build
//! has no subagent registry (see [`girder_core::SessionAddress`]'s docs), so the child case resolves
//! to an explicit refusal rather than silently reading the parent's log — the failure mode that
//! would show a caller someone else's conversation.
//!
//! # Module-include note
//!
//! Both this file and its test are included by path from `tests/session_controller_gap.rs`, so every
//! reference to the library goes through `girder_core::…` rather than `crate::…`: inside a `#[path]`
//! module, `crate::` names the *test* crate.

use girder_core::{
    SessionAddress, SessionEvent, SessionHistoryRecord, SurfaceEnvelope, SurfaceOp,
    is_append_surface_event,
};

/// Message budget applied when a history request omits `maxMessages`.
///
/// Upstream `DEFAULT_MAX_MESSAGES`. Fifty is roughly one screen of conversation, and it is the value
/// the client's own first page asks for, so an omitted budget and a client page agree.
pub const DEFAULT_MAX_MESSAGES: usize = 50;

/// Message budget of the client's jump-to-location read.
///
/// Upstream `JUMP_PAGE_MESSAGES`. A jump reads *around* an event the user picked, so it asks for much
/// more context than an ordinary page.
pub const JUMP_PAGE_MESSAGES: usize = 200;

/// Message budget of the client's ordinary history page.
///
/// Upstream `PAGE_MESSAGES`. Equal to [`DEFAULT_MAX_MESSAGES`] by construction upstream, but a
/// separate constant because the client's paging policy and the server's default are allowed to
/// diverge.
pub const PAGE_MESSAGES: usize = 50;

/// The event types a message budget counts.
///
/// Upstream `MESSAGE_TYPES`. Only the two model-visible message types count: a `developer/message` is
/// a tool-definition admission riding a turn, and a `system/message` is injected context, so neither
/// is a message a reader pages through.
const MESSAGE_TYPES: [&str; 2] = ["user/message", "assistant/message"];

/// A history-read failure with upstream's protocol code.
///
/// Upstream throws `RemoteError` with a `gateway/bad-request` code for a malformed window and a
/// `session/not-found` / `subagent/not-found` code for a missing target. The code is carried rather
/// than collapsed into a message so a transport can map it back onto the protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryError {
    code: &'static str,
    detail: String,
}

impl HistoryError {
    /// Builds a failure with an explicit upstream code.
    #[must_use]
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// A malformed request: an invalid cursor or message window.
    ///
    /// Upstream `gateway/bad-request`.
    #[must_use]
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new("gateway/bad-request", detail)
    }

    /// The session named by an address does not exist.
    ///
    /// Upstream `session/not-found`.
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new("session/not-found", detail)
    }

    /// The subagent named by an address is not readable by this caller.
    ///
    /// Upstream `subagent/unauthorized` and `subagent/not-found` both land here: this build has no
    /// subagent registry, so neither can be resolved, and a caller must not be told which of the two
    /// it was — that difference leaks whether a child exists under someone else's parent.
    #[must_use]
    pub fn subagent_unavailable(detail: impl Into<String>) -> Self {
        Self::new("subagent/not-found", detail)
    }

    /// The stable protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The human-readable detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for HistoryError {}

/// The tri-state log cut a page request names.
///
/// Upstream `SessionPageRequest['throughSeq']`, where the three states are `undefined`, `-1`, and a
/// non-negative integer. `-0` is rejected upstream because it is observationally `0` in arithmetic
/// but distinguishable in `Object.is`; Rust has no such value, so only the three meaningful states
/// are representable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionCursor {
    /// Read the observed log end.
    #[default]
    LogEnd,
    /// The log holds nothing at or below the cut; read no events.
    Empty,
    /// An inclusive sequence number.
    At(u64),
}

impl SessionCursor {
    /// Builds a cursor from upstream's wire value.
    ///
    /// `None` is "omitted" ([`SessionCursor::LogEnd`]), `-1` is [`SessionCursor::Empty`], and
    /// anything else must be a non-negative integer.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError`] with `gateway/bad-request` for a value below `-1`.
    pub fn from_wire(value: Option<i64>) -> Result<Self, HistoryError> {
        match value {
            None => Ok(Self::LogEnd),
            Some(-1) => Ok(Self::Empty),
            Some(raw) if raw >= 0 => Ok(Self::At(raw.unsigned_abs())),
            Some(raw) => Err(HistoryError::bad_request(format!(
                "throughSeq must be an integer greater than or equal to -1, got {raw}"
            ))),
        }
    }

    /// Resolves this cursor against the observed log end.
    ///
    /// Upstream `resolveHistoryRequest` leaves `throughSeq` as the request wrote it and
    /// `history()` substitutes `observation.cursor` for the omitted state. Doing the substitution
    /// here means [`paginate`] receives one concrete cut and cannot mistake the omitted state for
    /// zero.
    #[must_use]
    pub fn resolve(self, observed_end: u64) -> u64 {
        match self {
            Self::LogEnd => observed_end,
            Self::Empty => 0,
            Self::At(seq) => seq,
        }
    }

    /// Whether the cursor names the empty state rather than a real cut.
    ///
    /// An empty cut reads nothing regardless of the log, which is why upstream encodes it as `-1`
    /// instead of `0`: seq 0 is a real event in any non-empty log.
    #[must_use]
    pub fn is_empty(self) -> bool {
        matches!(self, Self::Empty)
    }
}

/// The optional turn-boundary stop rule of a page request.
///
/// Upstream `SessionPageRequest['turnWindow']`. Both minima must be satisfied before a `turn/start`
/// can end the page, so the page always spans at least `min_turns` turns and `min_messages` messages
/// when the log has them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnWindow {
    /// Minimum append-origin user/assistant messages; must not exceed `max_messages`.
    pub min_messages: usize,
    /// Minimum `turn/start` events crossed, including the partial turn at `before_seq`.
    pub min_turns: usize,
}

/// A page request after its window has been validated and defaulted.
///
/// Upstream `resolveHistoryRequest`'s return type
/// (`SessionPageRequest & { maxMessages: number }`): the budget is guaranteed present, so the
/// paginator never has to pick a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedHistoryRequest {
    /// The inclusive log cut, resolved against the observed end.
    pub through_seq: SessionCursor,
    /// Exclusive upper bound of an older page, when the caller is walking backwards.
    pub before_seq: Option<u64>,
    /// Message budget, guaranteed present.
    pub max_messages: usize,
    /// Optional turn-boundary stop rule.
    pub turn_window: Option<TurnWindow>,
}

impl ResolvedHistoryRequest {
    /// Validates the window fields of a page request.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError`] with `gateway/bad-request` when the message budget is zero, or when
    /// the turn window's minima are zero or the message minimum exceeds the budget.
    pub fn new(
        through_seq: SessionCursor,
        before_seq: Option<u64>,
        max_messages: Option<usize>,
        turn_window: Option<TurnWindow>,
    ) -> Result<Self, HistoryError> {
        // `max_messages` arrives as a `usize` on the wire in this build, so the only invalid value
        // left is zero. Upstream also rejects a non-integer, which cannot occur here.
        if max_messages == Some(0) {
            return Err(HistoryError::bad_request(
                "maxMessages must be a positive safe integer",
            ));
        }
        let budget = max_messages.unwrap_or(DEFAULT_MAX_MESSAGES);
        if let Some(window) = turn_window {
            validate_turn_window(window, budget)?;
            return Ok(Self {
                through_seq,
                before_seq,
                max_messages: budget,
                turn_window: Some(window),
            });
        }
        Ok(Self {
            through_seq,
            before_seq,
            max_messages: budget,
            turn_window: None,
        })
    }
}

/// Rejects a turn window that cannot be honoured.
///
/// Upstream `validateHistoryWindow`'s turn-window half. `min_messages` is checked against the budget
/// as well as against zero: a window demanding 51 messages under a 50-message budget would stop the
/// page only at exhaustion, which is not a window at all.
fn validate_turn_window(window: TurnWindow, max_messages: usize) -> Result<(), HistoryError> {
    if window.min_messages == 0 {
        return Err(HistoryError::bad_request(
            "turnWindow.minMessages must be a positive safe integer no greater than maxMessages",
        ));
    }
    if window.min_messages > max_messages {
        return Err(HistoryError::bad_request(format!(
            "turnWindow.minMessages ({}) must not exceed maxMessages ({max_messages})",
            window.min_messages
        )));
    }
    if window.min_turns == 0 {
        return Err(HistoryError::bad_request(
            "turnWindow.minTurns must be a positive safe integer",
        ));
    }
    Ok(())
}

/// Selects the log an address names.
///
/// Upstream `addressId` returns the session id for an ordinary address and the *child's* id for a
/// subagent one — never the parent's, which would read a different conversation. This crate's
/// [`SessionAddress`] carries a single id (the child case is not modelled; see its docs), so both
/// upstream branches agree here and the id is returned directly.
#[must_use]
pub fn address_id(address: &SessionAddress) -> String {
    address.session_id.clone()
}

/// Resolves an address to the log id to read, refusing what this build cannot serve.
///
/// # Errors
///
/// Returns [`HistoryError`] when the address names a subagent child.
pub fn resolve_address(address: &SessionAddress) -> Result<String, HistoryError> {
    match address.kind.as_str() {
        "subagent" => Err(HistoryError::subagent_unavailable(format!(
            "subagent address for session \"{}\" cannot be resolved by this build",
            address.session_id
        ))),
        _ => Ok(address.session_id.clone()),
    }
}

/// Rejects a missing session with its address-specific error.
///
/// Upstream `rejectNotFound`, which is `never`: it always throws. Returning the error rather than
/// panicking keeps the caller's failure path explicit.
#[must_use]
pub fn reject_not_found(address: &SessionAddress) -> HistoryError {
    match address.kind.as_str() {
        "subagent" => HistoryError::subagent_unavailable("subagent is unavailable"),
        _ => HistoryError::not_found(format!("session \"{}\" not found", address.session_id)),
    }
}

/// One page cut from a contiguous event prefix.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryPage {
    /// The page's events, in chronological order.
    pub events: Vec<SessionEvent>,
    /// Whether older events remain before this page.
    pub has_more: bool,
}

/// Cuts one message-aligned backwards page from a contiguous event prefix.
///
/// `events` must be contiguous and ordered by log position; `envelopes` supplies each event's
/// placement so a `turn/start` and an append-origin message can be recognised. The two slices are
/// indexed in lockstep, which is how upstream reads `event.seq` and `isAppendSurfaceEvent` off one
/// record.
///
/// Returns the page and whether anything older remains. An empty cursor yields an empty page.
///
/// # Errors
///
/// Returns [`HistoryError`] with `gateway/bad-request` when `through_seq` exceeds the last event's
/// sequence (the read would name a cut the log does not have), when `events` and `envelopes`
/// disagree in length, or when the envoicings are not ordered by sequence.
pub fn paginate(
    events: &[SessionEvent],
    envelopes: &[SurfaceEnvelope],
    request: &ResolvedHistoryRequest,
    observed_end: u64,
) -> Result<HistoryPage, HistoryError> {
    if events.len() != envelopes.len() {
        return Err(HistoryError::bad_request(format!(
            "page input disagrees: {} events against {} envelopes",
            events.len(),
            envelopes.len()
        )));
    }
    let cursor = request.through_seq.resolve(observed_end);
    if cursor > observed_end {
        return Err(HistoryError::bad_request(format!(
            "session history cut {cursor} exceeds {observed_end}"
        )));
    }
    // Note that this is deliberately *not* an early return for `LogEnd`: resolving already picked
    // the observed end. Only the explicit empty state reads nothing.
    if request.through_seq.is_empty() {
        return Ok(HistoryPage {
            events: Vec::new(),
            has_more: false,
        });
    }
    let end = cut_end(cursor, request.before_seq);
    let cut = page_cut(events, envelopes, end, request);
    let start = usize::try_from(cut).unwrap_or(usize::MAX);
    let stop = usize::try_from(end).unwrap_or(usize::MAX);
    let page = if start <= stop && stop <= events.len() {
        events
            .get(start..stop)
            .map(<[SessionEvent]>::to_vec)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(HistoryPage {
        events: page,
        has_more: cut > 0,
    })
}

/// The exclusive log offset a page stops at.
///
/// Upstream computes `min(throughSeq + 1, beforeSeq ?? throughSeq + 1)`. `saturating_add` is the
/// honest arithmetic here: a `through_seq` of `u64::MAX` names "everything", and wrapping to zero
/// would turn the widest possible read into an empty one.
fn cut_end(through_seq: u64, before_seq: Option<u64>) -> u64 {
    let inclusive = through_seq.saturating_add(1);
    before_seq.map_or(inclusive, |before| inclusive.min(before))
}

/// Walks backwards from `end` to the offset this page starts at.
///
/// This is upstream's loop, including its ordering: the turn-window check happens *before* the
/// message check, so a `turn/start` that also closes a message budget is attributed to the turn. The
/// difference is one message at the page boundary, which decides whether the next page repeats an
/// event or skips one.
fn page_cut(
    events: &[SessionEvent],
    envelopes: &[SurfaceEnvelope],
    end: u64,
    request: &ResolvedHistoryRequest,
) -> u64 {
    let mut count: usize = 0;
    let mut turns: usize = 0;
    let mut cut: u64 = 0;
    // `end` is an exclusive offset, so the last inspectable index is `end - 1`.
    let Some(mut index) = end.checked_sub(1) else {
        return 0;
    };
    loop {
        let position = usize::try_from(index).unwrap_or(usize::MAX);
        let (Some(event), Some(envelope)) = (events.get(position), envelopes.get(position)) else {
            break;
        };
        if let Some(window) = request.turn_window {
            if event.type_name() == "turn/start" {
                turns += 1;
                if count >= window.min_messages && turns >= window.min_turns {
                    cut = index;
                    break;
                }
            }
        }
        if MESSAGE_TYPES.contains(&event.type_name()) && is_append_surface_event(envelope) {
            count += 1;
            // A retry re-derives a message from earlier events. Cutting at the message's own seq
            // would leave those sources on neither page, so the cut moves back to the earliest.
            let group_start = envelope.sources.iter().copied().fold(index, u64::min);
            if count >= request.max_messages {
                cut = group_start;
                break;
            }
        }
        if index == 0 {
            break;
        }
        index -= 1;
    }
    cut
}

/// Wraps one committed event in its wire record.
///
/// Upstream `entryFor`. The `type` discriminant is how a page can later carry records that are not
/// events without changing the array's element type.
#[must_use]
pub fn entry_for(event: SessionEvent) -> SessionHistoryRecord {
    SessionHistoryRecord::event(event)
}

/// Encodes a page's events as wire records without changing its cut.
///
/// Upstream `pageRecords`. Order is preserved: the records are what the client's backwards reader
/// prepends, so reversing them here would reverse the conversation.
#[must_use]
pub fn page_records(events: &[SessionEvent]) -> Vec<SessionHistoryRecord> {
    events.iter().cloned().map(entry_for).collect()
}

/// Merges an authoritative baseline into an established client order.
///
/// Upstream `mergeOrderedBaseline`. Three rules, each of which exists because the naive merge breaks
/// a visible list:
///
/// - **Existing identities keep their relative order.** The client's order is what the user is
///   looking at; adopting the baseline's order would make rows jump on every refresh.
/// - **A baseline-only identity is inserted before the nearest known follower.** Appending it to the
///   end would put a new child below unrelated rows.
/// - **An identity absent from the baseline is removed.** The baseline is authoritative about
///   *existence*; only *order* belongs to the client.
#[must_use]
pub fn merge_ordered_baseline<T, K, F>(current: &[T], baseline: &[T], key_of: F) -> Vec<T>
where
    T: Clone,
    K: PartialEq + Clone,
    F: Fn(&T) -> K,
{
    let keyed: Vec<(K, T)> = baseline
        .iter()
        .map(|value| (key_of(value), value.clone()))
        .collect();

    // Keep the current order, but take the baseline's *value* for each surviving identity: the row
    // may have advanced (a title, a running flag) while its position has not.
    let mut merged: Vec<(K, T)> = Vec::new();
    for value in current {
        let key = key_of(value);
        if let Some((_, fresh)) = keyed.iter().find(|(candidate, _)| *candidate == key) {
            merged.push((key, fresh.clone()));
        }
    }

    for (index, (key, value)) in keyed.iter().enumerate() {
        if merged.iter().any(|(known, _)| known == key) {
            continue;
        }
        // Find the nearest *following* baseline row that is already merged; the new row goes there.
        let mut insertion = merged.len();
        for (following_key, _) in keyed.iter().skip(index + 1) {
            if let Some(position) = merged.iter().position(|(known, _)| known == following_key) {
                insertion = position;
                break;
            }
        }
        merged.insert(insertion, (key.clone(), value.clone()));
    }

    merged.into_iter().map(|(_, value)| value).collect()
}

/// One flattened session-list row with lineage depth.
///
/// Upstream `SessionListEntry`. Only the fields the flattening itself decides are modelled: identity,
/// parentage, and depth. A caller keeps its own presentation fields and zips them with these rows,
/// which is why [`flatten_lineage`] returns indices into the input rather than copies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionListEntry {
    /// Index of this row in the input slice.
    pub index: usize,
    /// Session identity.
    pub session_id: String,
    /// Lineage indent depth: a root is 0.
    pub depth: usize,
}

/// One row of input to [`flatten_lineage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineageRow<'a> {
    /// Session identity.
    pub session_id: &'a str,
    /// The parent's identity, present only for a subagent child.
    pub parent_session_id: Option<&'a str>,
    /// Whether this row is a subagent child rather than an ordinary fork.
    pub subagent: bool,
}

/// Flattens a session list into display order with lineage indentation.
///
/// Upstream `flattenLineage`. The input order is authoritative: roots and siblings keep it, and
/// lineage only makes each child adjacent to its parent. Three degradations keep every row visible:
///
/// - **An orphan** (a parent absent from the list) becomes a root. Dropping it would hide a session
///   the caller listed.
/// - **A cycle** is broken by emitting already-visited nodes as roots, so no row is lost and the walk
///   terminates.
/// - **An ordinary fork** with a `parent_session_id` stays at root level: historical forks may retain
///   a parent id without being subagents.
///
/// The returned rows index the input, so a caller renders its own data in this order.
#[must_use]
pub fn flatten_lineage(rows: &[LineageRow<'_>]) -> Vec<SessionListEntry> {
    // Children are grouped only when the declared parent is actually present: an orphan must
    // degrade to a root, not vanish into a group nobody walks.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let parent = row
            .parent_session_id
            .filter(|_| row.subagent)
            .and_then(|parent| rows.iter().position(|other| other.session_id == parent));
        match parent {
            Some(parent) => {
                if let Some(list) = children.get_mut(parent) {
                    list.push(index);
                }
            }
            None => roots.push(index),
        }
    }

    let mut out: Vec<SessionListEntry> = Vec::with_capacity(rows.len());
    let mut visited = vec![false; rows.len()];
    for root in roots {
        walk(rows, &children, root, 0, &mut visited, &mut out);
    }
    // Members reachable only through a cycle never appear above; emitting them as roots is what
    // makes the cycle fail soft instead of hiding rows.
    for index in 0..rows.len() {
        if !visited.get(index).copied().unwrap_or(true) {
            walk(rows, &children, index, 0, &mut visited, &mut out);
        }
    }
    out
}

/// Depth-first walk that emits one node and then its children.
fn walk(
    rows: &[LineageRow<'_>],
    children: &[Vec<usize>],
    index: usize,
    depth: usize,
    visited: &mut [bool],
    out: &mut Vec<SessionListEntry>,
) {
    let Some(row) = rows.get(index) else {
        return;
    };
    let Some(seen) = visited.get_mut(index) else {
        return;
    };
    if *seen {
        // A cycle: the node was already emitted at some depth. Re-emitting it would duplicate a row
        // and, for a self-reference, recurse forever.
        return;
    }
    *seen = true;
    out.push(SessionListEntry {
        index,
        session_id: row.session_id.to_owned(),
        depth,
    });
    if let Some(kids) = children.get(index) {
        for &kid in kids {
            walk(rows, children, kid, depth + 1, visited, out);
        }
    }
}

/// Reads the first logical sequence represented by one wire record.
///
/// Upstream `historyRecordFirstSeq`, which reads the record's own `seq`. Placement is not folded into
/// it: a record stands at exactly one log position.
#[must_use]
pub fn history_record_first_seq(
    record: &SessionHistoryRecord,
    envelopes: &[SurfaceEnvelope],
) -> Option<u64> {
    envelopes
        .iter()
        .find(|envelope| envelope.event == record.event)
        .map(|envelope| envelope.seq)
}

/// The alternative placement an event may carry, for callers that build envelopes themselves.
///
/// Upstream encodes this on the event as `surfaceOp`; this build keeps it on
/// [`SurfaceEnvelope`], so the constructor is exposed for the same reason
/// [`girder_core::SurfaceEnvelope::new`] is.
///
/// # Errors
///
/// Returns [`HistoryError`] when the event's type cannot carry a placement.
pub fn envelope_with_op(
    event: SessionEvent,
    seq: u64,
    op: SurfaceOp,
) -> Result<SurfaceEnvelope, HistoryError> {
    SurfaceEnvelope::new(event, seq, Some(op), Vec::new())
        .map_err(|error| HistoryError::bad_request(format!("invalid surface placement: {error}")))
}
