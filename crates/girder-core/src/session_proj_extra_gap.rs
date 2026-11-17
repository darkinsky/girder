//! The `turnOutline` projection unit: every started turn with the `turn/start` seq a jump can page
//! through, plus bounded prompt and response previews.
//!
//! Upstream path: `packages/session/session-turn-outline/src` — 229 lines across four sources
//! (`projection.ts` 177, `types.ts` 68, `index.ts` 27, `client.ts` 8, less shared boilerplate).
//! The package is one capability in three layers, and only the middle one carries logic:
//!
//! | upstream file | kind | what it is |
//! |---|---|---|
//! | `types.ts` | pure declaration | `TurnOutlineEntry`, `TurnOutlineState`, and the `turnOutline` projection-key augmentation (marked `SessionProjectionSummaryExcluded`) |
//! | `projection.ts` | **pure fold over events** | [`preview`], the two zod schemas, and `turnOutlineProjectionDefinition` |
//! | `index.ts` | service | a Cordis plugin: `name`, `inject: ['sessionProjections']`, one `register` |
//! | `client.ts` | re-export | `export type * from './types.ts'` |
//!
//! `index.ts` and `client.ts` hold no logic: the plugin registers a definition and the client
//! namespace re-exports the same types. So this module ports the fold, both schemas, the registry
//! adapter, and the plugin's two constants ([`NAME`], [`INJECT`]).
//!
//! # The gap this closes, stated as evidence rather than as an impression
//!
//! `grep -rn 'turnOutline'` over `crates/` returns exactly **one** hit, and it is a comment:
//!
//! ```text
//! $ grep -rn 'turnOutline' crates/
//! crates/girder-core/src/session_timeline.rs:9://! Upstream (`packages/session/session-turn-outline`) registers a `turnOutline` projection: for every
//! ```
//!
//! The key has no producer, no constant, no fold, and no test. That one comment is also the
//! admission: [`crate::session_timeline`] says in its own header that "this build has no timeline
//! projection", and [`crate::session_outline`] compensates by folding `&[SessionEvent]` on demand.
//!
//! The compensation is real but incomplete, and the incompleteness is structural rather than
//! incidental:
//!
//! - [`crate::TimelineSegment`] carries `turn`, `summary`, `tools`, and `steps`. It does **not**
//!   carry the `turn/start` sequence number, so the one thing upstream's rail exists for — "click an
//!   unloaded turn, page history back through its seq" — is not expressible from a `Segment` at all.
//!   [`crate::session_state_rpc`] line 360 puts `session_outline(&record.events)` on the wire and a
//!   client that wants to jump has to re-scan the log to find the seq the row it just rendered was
//!   anchored on.
//! - [`crate::session_controller_gap`] already models the destination: [`through_seq`] is the
//!   inclusive cut a page request resolves against, and [`crate::session_controller_gap::TurnWindow`]
//!   stops a page on a `turn/start`. So the paging half exists and consumes a seq; nothing produces
//!   the seq per turn.
//! - The previews are the second half. [`crate::session_timeline`] deliberately previews the
//!   *request* and adds tool names, and says so: "upstream previews the *response*, this module
//!   previews the *request*". A rail card shows both, and the response preview is only knowable at
//!   `turn/end` — which is exactly what [`TurnOutlineState::draft`] buffers.
//!
//! So this is the shape the task's brief calls out: **the consumer exists and the producer does
//! not.** The difference from `sessionListMetadata` is only that the consumer here is a documented
//! absence ([`crate::session_timeline`] says so in prose) rather than a `.value("…")` read that
//! always misses.
//!
//! # Why the entry is anchored on `turn/start` and not on the prompt
//!
//! Upstream anchors on `turn/start` because its seq is the *load-through target*: the loop appends
//! `turn/start` before the turn's prompt and steps, so a window paged back through that seq contains
//! the whole turn. Anchoring on the `user/message` would land the window one event late and show a
//! turn whose opening line is missing.
//!
//! That ordering is not something the log guarantees in this crate: [`crate::session_timeline`]
//! documents that "this build writes the prompt first so it survives a crash before the model
//! answers". So the fallback here is the same one the timeline uses — a prompt that arrives before
//! its boundary is attached to the turn that opens next — and the anchor still comes from
//! `turn/start`, never from the prompt. A `turn/start` that does not advance the turn number keeps
//! the outline sorted and lands the previews on the standing entry, which is what makes a retried
//! turn idempotent.
//!
//! # Four deliberate differences from upstream, each visible rather than silent
//!
//! 1. **The state carries a `pending` buffer, so `stateVersion` is 3 rather than 2.** This is the
//!    one difference a reader must know about, because it changes a durable value.
//!    [`TurnOutlineState::pending`] holds a human prompt that arrived *before* the boundary that owns
//!    it — which is every prompt this crate writes, since `runtime.rs` records the prompt before it
//!    spawns the loop. Upstream has no such field because its loop opens `turn/start` first and a
//!    prompt is therefore never pre-boundary. A fold without the buffer would leave every turn in
//!    this crate's logs with an empty title. The field is state-only: the wire view is upstream's
//!    `turns` array exactly, so no client sees it, and the version bump exists so a v2 checkpoint
//!    row is discarded rather than read as a v3 state with the field silently missing.
//! 2. **Previews count code points, not UTF-16 units.** Upstream's budget is `text.length`, which is
//!    UTF-16 code units — one emoji costs two. Counting code points gives a CJK or emoji preview the
//!    same rendered width it would have upstream and never splits a surrogate pair. The consequence
//!    is that a preview can be up to 50 code points where upstream's is up to 50 UTF-16 units; the
//!    budgets are budgets, not exact lengths, in both implementations.
//! 3. **`seq` is passed to `apply` rather than read off the event.** [`girder_core::SessionEvent`]
//!    carries no seq — the position *is* the sequence — which is exactly the argument
//!    [`girder_core::Projection::apply`] already takes. Upstream reads `event.seq`; this reads the
//!    parameter. Same value, and here it cannot disagree with the log.
//! 4. **The draft-only identity gate is preserved by leaving `turns` equal.** The change feed compares
//!    the published view, and the view is `turns` alone; an `assistant/message` that only moves
//!    `draft` produces a state whose `turns` is identical, so a consumer that diffs the wire sees no
//!    change between turn boundaries. Upstream gets this from reference identity; here it is value
//!    equality, which is the same answer for every state a correct fold can produce.
//!
//! # What this module is careful not to duplicate
//!
//! - **Not the timeline.** [`crate::session_outline`] answers "what did each turn do" — a summary
//!   line, the tools, the step count. This answers "where is each turn and what did it look like".
//!   A caller rendering a sidebar wants both and neither replaces the other; [`merge_rail_items`]
//!   is the one place they meet, and it is a merge rather than a re-derivation for that reason.
//! - **Not the title.** [`girder_core::TitleProjection`] folds `session/title` events into the
//!   session's *name*. A turn's prompt preview is what the person said, not what the session is
//!   called.
//! - **Not the window.** [`crate::session_window_gap`] owns the loaded event window and the
//!   assistant-stream fold. This module names turns the window does *not* hold, which is why it can
//!   answer before any window exists.
//! - **Not the pager.** [`crate::session_controller_gap`] resolves [`through_seq`] against the
//!   observed end. This module produces the seq to pass it.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use serde::{Deserialize, Serialize};

use girder_core::{ContentBlock, MessageSource, Projection, ProjectionSeq as Seq, SessionEvent};

/// The Cordis plugin name upstream registers under.
pub const NAME: &str = "session-turn-outline";

/// The services the plugin needs before it can activate.
///
/// The projection registry is the plugin's whole purpose; without it the fiber stays pending.
pub const INJECT: [&str; 1] = ["sessionProjections"];

/// The projection key a session's turn outline is published under.
pub const KEY: &str = "turnOutline";

/// The persisted-state version.
///
/// Upstream's current value is `2` (the entry gained `response` and the state gained `draft` relative
/// to v1). This port writes `3` because its state carries one field upstream's does not, and a
/// v2-written checkpoint cannot be read as a v3 state without silently dropping it. See
/// [`TurnOutlineState::pending`] for the reason the field exists. The *wire* view is unchanged from
/// upstream — `turns` alone — so nothing a client sees depends on this number; only the durable row
/// does, which is exactly the scope `stateVersion` is defined to cover.
pub const STATE_VERSION: u64 = 3;

/// Prompt budget: one rail-card line (13px over ~276px), ASCII worst case included.
pub const PROMPT_PREVIEW_LIMIT: usize = 50;

/// Response budget: three rail-card lines (12px over ~276px).
pub const RESPONSE_PREVIEW_LIMIT: usize = 120;

/// The character a clipped preview ends with.
///
/// One character rather than `"..."`: the dots spend three of a small budget, and the ellipsis is a
/// single column in every font that has it. The same choice [`crate::session_timeline`] makes for
/// its summaries.
const ELLIPSIS: char = '…';

/// One started turn's outline facts, independent of what a client has paged in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnOutlineEntry {
    /// Host-assigned turn number (the `turn/start` payload).
    pub turn: u64,
    /// The turn's `turn/start` event seq — paging a window back through this seq loads the whole
    /// turn.
    pub seq: u64,
    /// Bounded first-human-prompt preview (one rail-card line); `""` until an eligible prompt lands.
    pub prompt: String,
    /// Bounded final-response preview (up to three rail-card lines); `""` until the turn ends with
    /// assistant text.
    pub response: String,
}

/// Fold state: the served entries, the open turn's response draft, and a prompt that arrived before
/// its boundary.
///
/// The draft buffers the newest text-bearing assistant message until `turn/end` commits it, and the
/// wire view projects only [`TurnOutlineState::turns`] — draft-only applies keep that field equal, so
/// the change feed stays quiet between turn boundaries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnOutlineState {
    /// Started turns in ascending turn order.
    pub turns: Vec<TurnOutlineEntry>,
    /// Newest text-bearing assistant preview of the open turn; `""` outside one.
    pub draft: String,
    /// A human prompt that arrived before the boundary that owns it, held until that `turn/start`.
    ///
    /// This field is the one place the port's state differs from upstream's, and it exists because
    /// the *log order* differs. Upstream's loop opens `turn/start` before claiming the input, so a
    /// prompt is always inside the turn it belongs to. This crate writes the prompt first — see
    /// `crates/girder-core/src/runtime.rs`, which records the deviation deliberately so a prompt that
    /// reached the server survives a crash before the model answered. A fold that dropped a
    /// pre-boundary prompt would therefore leave *every* turn in this crate's logs with an empty
    /// title, which is the bug this field fixes rather than works around.
    ///
    /// It is state-only, like [`TurnOutlineState::draft`]: the wire view omits it, because a prompt
    /// whose turn has not started yet is not a rail mark — there is no seq to jump to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pending: String,
}

impl TurnOutlineState {
    /// The empty log's outline: no turns started, no draft, no buffered prompt.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            turns: Vec::new(),
            draft: String::new(),
            pending: String::new(),
        }
    }

    /// The last started turn, which is the one still accepting a prompt and a response.
    #[must_use]
    pub fn last_turn(&self) -> Option<&TurnOutlineEntry> {
        self.turns.last()
    }

    /// One turn's entry, addressed by its number.
    ///
    /// A scan rather than a map lookup: the vector is ascending and small (one row per turn of one
    /// conversation), so a binary search would be more code for the same bound.
    #[must_use]
    pub fn entry(&self, turn: u64) -> Option<&TurnOutlineEntry> {
        self.turns.iter().find(|entry| entry.turn == turn)
    }

    /// The wire view upstream publishes: the entries alone, without the draft.
    ///
    /// A clone rather than a borrow because the value crosses a JSON boundary; the caller that only
    /// wants to render can read [`TurnOutlineState::turns`] directly.
    #[must_use]
    pub fn to_wire(&self) -> serde_json::Value {
        serde_json::to_value(&self.turns).unwrap_or(serde_json::Value::Null)
    }

    /// The whole persisted state as JSON, which is what a checkpoint row carries.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// Reads a persisted state back, enforcing upstream's schema.
    ///
    /// Returns `None` for anything the schema rejects: a missing field, an extra field, a preview
    /// past its budget, a `draft` past its budget, or turn numbers that are not strictly increasing.
    /// A caller keeps the state it held — substituting [`TurnOutlineState::new`] would silently
    /// delete every row a user was looking at, and a malformed row is not evidence that the log had
    /// no turns.
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let state: Self = serde_json::from_value(value.clone()).ok()?;
        state.validate().then_some(state)
    }

    /// Whether every entry and the buffers satisfy the wire schema.
    fn validate(&self) -> bool {
        if self.draft.chars().count() > RESPONSE_PREVIEW_LIMIT
            || self.pending.chars().count() > PROMPT_PREVIEW_LIMIT
        {
            return false;
        }
        let mut previous: Option<u64> = None;
        for entry in &self.turns {
            if let Some(previous) = previous
                && entry.turn <= previous
            {
                return false;
            }
            previous = Some(entry.turn);
            if entry.prompt.chars().count() > PROMPT_PREVIEW_LIMIT
                || entry.response.chars().count() > RESPONSE_PREVIEW_LIMIT
            {
                return false;
            }
        }
        true
    }
}

/// Collapses every run of whitespace to one space and trims the ends.
///
/// Upstream's `replace(/\s+/g, ' ').trim()`. Rust's [`char::is_whitespace`] is Unicode-aware in the
/// same way JavaScript's `\s` is for the code points a message realistically carries, so a preview of
/// indented source and blank lines comes out as one line in both implementations.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(character);
    }
    out
}

/// Space-joins the text blocks of a message, collapses whitespace, and caps the result.
///
/// Upstream's `preview(content, limit)`, with the per-block bound preserved: the fold runs on every
/// message event, so a single multi-megabyte block must not be concatenated — and whitespace-
/// normalized — whole for a preview this short. A block longer than `limit * 2` is clipped to
/// `limit * 2` and marks the preview as clipped; a message whose accumulated text already fills the
/// budget stops reading blocks at all.
///
/// A clipped preview always ends with [`ELLIPSIS`], and so does one whose text ran out of blocks but
/// was already over the budget — the second case is what makes "there is more that this preview did
/// not read" honest rather than inferred from a trailing character that the real text might have
/// contained.
#[must_use]
pub fn preview(content: &[ContentBlock], limit: usize) -> String {
    let budget = limit.saturating_mul(2);
    let mut text = String::new();
    let mut unread = false;
    for block in content {
        let ContentBlock::Text { text: block_text } = block else {
            continue;
        };
        if text.chars().count() >= budget {
            unread = true;
            break;
        }
        let clipped = block_text.chars().count() > budget;
        let chunk: String = if clipped {
            block_text.chars().take(budget).collect()
        } else {
            block_text.clone()
        };
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&chunk);
        if clipped {
            unread = true;
            break;
        }
    }
    let normalized = collapse_whitespace(&text);
    // `limit - 1` leaves room for the ellipsis, which is why a preview is never `limit` wide after
    // clipping: the caller's clamp is exact and the marker must fit inside it.
    let head = limit.saturating_sub(1);
    if normalized.chars().count() > head {
        let clipped: String = normalized.chars().take(head).collect();
        return trimmed_with_ellipsis(&clipped);
    }
    if unread {
        return format!("{normalized}{ELLIPSIS}");
    }
    normalized
}

/// Appends the ellipsis to an already-trimmed prefix.
fn trimmed_with_ellipsis(clipped: &str) -> String {
    format!("{}{ELLIPSIS}", clipped.trim_end())
}

/// Advances the outline by one committed event.
///
/// Upstream's `apply`, unchanged in behaviour. Every uninteresting event returns the same state, and
/// a draft-only change keeps `turns` equal so the published view does not move.
///
/// `seq` is the position of `event` in the log, which is what an entry anchors on. A caller replaying
/// a stored log must pass the row's seq, exactly as it must for every other projection.
#[must_use]
pub fn apply(state: &TurnOutlineState, event: &SessionEvent, seq: Seq) -> TurnOutlineState {
    match event {
        SessionEvent::TurnStart { turn } => {
            // Order guard: a boundary that does not advance the turn number keeps the outline
            // sorted, and a retried turn's previews land on the standing entry.
            if state.last_turn().is_some_and(|last| *turn <= last.turn) {
                return state.clone();
            }
            let mut next = state.clone();
            next.turns.push(TurnOutlineEntry {
                turn: *turn,
                // A prompt written before its boundary — which is the order this crate's runtime
                // uses — becomes the new turn's title. This is the same hand-off
                // [`crate::session_timeline`] performs with its own pending buffer, and it is why
                // that buffer is state rather than a local: the boundary that claims it is a later
                // event.
                prompt: std::mem::take(&mut next.pending),
                seq,
                response: String::new(),
            });
            next.draft = String::new();
            next
        }
        SessionEvent::UserMessage { message } => {
            // Only the person's own words open a turn. The crate-wide rule is `User { form: None }`
            // — see [`crate::session_timeline`], [`crate::session_digest`], and
            // [`crate::session_preview`], which all apply it for the same reason: a relayed peer
            // message, an injected notice, a compaction summary, and the system prompt all arrive on
            // the user role, and none of them is what the person asked for. `form: Some(_)` is the
            // marker that distinguishes a context injection from the real prompt, because upstream's
            // `ContextFormed` producers always set one and [`girder_core::Message::user`] never does.
            if !matches!(message.source, MessageSource::User { form: None }) {
                return state.clone();
            }
            let prompt = preview(&message.content, PROMPT_PREVIEW_LIMIT);
            if prompt.is_empty() {
                return state.clone();
            }
            let Some(last) = state.last_turn().cloned() else {
                // Before the boundary: this is the prompt of the turn about to open. Only the first
                // is kept — a queued burst is one request, and the first message is the one that
                // named it.
                if !state.pending.is_empty() {
                    return state.clone();
                }
                let mut next = state.clone();
                next.pending = prompt;
                return next;
            };
            // Only the newest turn can still be waiting for its opening prompt; a later human
            // message in the same turn (steering) keeps the first preview.
            if !last.prompt.is_empty() {
                return state.clone();
            }
            let mut next = state.clone();
            let last = next.turns.last_mut().expect("a last turn was just read");
            last.prompt = prompt;
            next
        }
        SessionEvent::AssistantMessage { message } => {
            // Newest text-bearing message wins; the buffer commits at `turn/end`.
            let draft = preview(&message.content, RESPONSE_PREVIEW_LIMIT);
            if draft.is_empty() || draft == state.draft {
                return state.clone();
            }
            let mut next = state.clone();
            next.draft = draft;
            next
        }
        SessionEvent::TurnEnd { .. } => {
            if state.draft.is_empty() {
                return state.clone();
            }
            // The buffer is cleared whatever the boundary closes: an unpaired `turn/end` leaves a
            // stale draft behind, and a later `turn/end` would otherwise commit a preview that
            // belongs to a turn that no longer exists.
            let mut next = state.clone();
            let draft = std::mem::take(&mut next.draft);
            if let Some(last) = next.turns.last_mut()
                && last.response != draft
            {
                last.response = draft;
            }
            next
        }
        _ => state.clone(),
    }
}

/// Folds a whole log into its outline, in seq order.
///
/// A convenience for the callers that have the events in hand — a replay, a test, an importer — and
/// the same result the registry reaches by driving [`apply`] one event at a time. The first event is
/// seq 1, because a seq of 1 is what the log's first row carries and an entry that anchored on 0
/// would page to nothing.
#[must_use]
pub fn fold(events: &[SessionEvent]) -> TurnOutlineState {
    let mut state = TurnOutlineState::new();
    for (index, event) in events.iter().enumerate() {
        // Seq is 1-based: the event at index `i` sits at seq `i + 1`.
        let seq = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        state = apply(&state, event, seq);
    }
    state
}

/// The registerable `turnOutline` unit.
///
/// A zero-sized definition, because the fold is a pure function of the log: there is no clock, no
/// source, and no configuration to hold. Registering it is the whole adapter, which is exactly what
/// upstream's plugin does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnOutlineProjection;

impl TurnOutlineProjection {
    /// The unit itself.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Projection for TurnOutlineProjection {
    fn key(&self) -> &'static str {
        KEY
    }

    fn init(&self) -> serde_json::Value {
        TurnOutlineState::new().to_json()
    }

    fn apply(&self, state: serde_json::Value, event: &SessionEvent, seq: Seq) -> serde_json::Value {
        // A state that does not parse is left untouched rather than replaced by `init`: rewriting it
        // would silently discard a fold that was almost readable, and the registry's cursor has
        // already moved past the event that would have rebuilt it.
        let Some(current) = TurnOutlineState::from_json(&state) else {
            return state;
        };
        apply(&current, event, seq).to_json()
    }

    fn in_summary(&self) -> bool {
        // Upstream declares `summary: false`: the outline grows with the whole log and no list row
        // renders it, so a summary payload that carried it would pay for every turn on every
        // listing and use none.
        false
    }
}

impl girder_core::VersionedProjection for TurnOutlineProjection {
    fn state_version(&self) -> u64 {
        STATE_VERSION
    }
}

/// Where a rail mark takes the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RailAnchor {
    /// The turn is inside the loaded window; scroll to the node key.
    Loaded {
        /// The loaded node's key.
        key: String,
    },
    /// The turn is outside the loaded window; page history through this seq first.
    Unloaded {
        /// The turn's `turn/start` seq.
        seq: u64,
    },
}

/// One rail mark after the outline and the loaded window have been merged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RailItem {
    /// Turn number the mark addresses.
    pub turn: u64,
    /// Bounded prompt preview, loaded window first, outline fallback.
    pub prompt: String,
    /// Bounded response preview, loaded window first, outline fallback.
    pub response: String,
    /// How the rail reaches the turn.
    pub anchor: RailAnchor,
}

/// One turn the client's loaded window already holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedRailItem {
    /// Turn number.
    pub turn: u64,
    /// The node key to scroll to.
    pub anchor_key: String,
    /// The window's own prompt preview; `""` when it has none.
    pub prompt: String,
    /// The window's own response preview; `""` when it has none.
    pub response: String,
}

/// Merges the host outline with the loaded rail items into the full ladder.
///
/// Upstream `client/ui-chat`'s `mergeTurnRailItems`: a turn present in both sides keeps the loaded
/// anchor, taking an outline preview only where the window's own is empty (a mid-turn window head, or
/// a turn whose loaded nodes carry no text); turns on one side only pass through. The result ascends
/// by turn.
///
/// This is the one place the two sources meet, and the merge rather than a re-derivation is the
/// point: the outline knows every turn, the window knows richer previews for the turns it holds, and
/// nothing but a merge can hold both facts at once.
#[must_use]
pub fn merge_rail_items(loaded: &[LoadedRailItem], outline: &TurnOutlineState) -> Vec<RailItem> {
    let mut merged: Vec<RailItem> = outline
        .turns
        .iter()
        .map(|entry| RailItem {
            turn: entry.turn,
            prompt: entry.prompt.clone(),
            response: entry.response.clone(),
            anchor: RailAnchor::Unloaded { seq: entry.seq },
        })
        .collect();
    for item in loaded {
        let prompt = if item.prompt.is_empty() {
            outline
                .entry(item.turn)
                .map(|entry| entry.prompt.clone())
                .unwrap_or_default()
        } else {
            item.prompt.clone()
        };
        let response = if item.response.is_empty() {
            outline
                .entry(item.turn)
                .map(|entry| entry.response.clone())
                .unwrap_or_default()
        } else {
            item.response.clone()
        };
        let replacement = RailItem {
            turn: item.turn,
            prompt,
            response,
            anchor: RailAnchor::Loaded {
                key: item.anchor_key.clone(),
            },
        };
        match merged.binary_search_by_key(&item.turn, |row| row.turn) {
            Ok(index) => merged[index] = replacement,
            Err(index) => merged.insert(index, replacement),
        }
    }
    merged
}
