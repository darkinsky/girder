//! Filtering a session list by structural properties.
//!
//! [`crate::session_query`] answers "which session contains this word", which needs the full event
//! log. This answers "which sessions are worth opening at all" — recent ones, long ones, ones that
//! actually ran a tool — and a list view asks that question on every keystroke of a filter box. Those
//! are different costs: a text search has to decode every event of every candidate, while a
//! structural filter over metadata decodes no conversation at all.
//!
//! # Why a summary rather than the session itself
//!
//! That cost difference is the whole reason [`SessionSummary`] exists. [`crate::SessionRecord`]
//! carries its events, and — as its own documentation says — [`crate::SessionCatalog::list`] leaves
//! `events` empty precisely because decoding every event of every session to answer a list query is
//! O(sessions x events) of work. A filter that took a `SessionRecord` would either force the list
//! path to materialize logs it deliberately does not read, or silently match nothing because the
//! count it wanted was `0` on every row.
//!
//! So the input here is the flat handful of facts a filter can decide on, produced once per row while
//! the catalog already has the file open or the row in hand:
//!
//! - `id` and `modified_ms` come straight from the catalog row (`SessionRecord::id`,
//!   `SessionRecord::updated_at`, or [`crate::remote::SessionSummary::session_id`] /
//!   `updated_at` on the remote path).
//! - `title` is the stored title when there is one, else the last [`crate::SessionEvent::SessionTitle`]
//!   in the log, else a title derived from the first prompt ([`crate::session_title::derive`]) — the
//!   same fallback order `session_lifecycle_rpc` uses. It stays an `Option` because a session can
//!   legitimately have no title at all.
//! - `events` is [`crate::session::SessionStore::event_count`], which exists so a caller that needs
//!   the length does not have to clone the events to get it.
//! - `tool_calls` is [`crate::session_stats::Stats::tool_calls`], a fold that already exists because
//!   "how much did this conversation do" is a separate question with its own answer.
//!
//! A caller that has an index supplies these from the index and pays neither cost. A caller walking
//! the directory pays exactly one pass per session, which is the price of the metadata, not of the
//! filter.
//!
//! # Why structured fields rather than a query string
//!
//! Same reasoning as `session_query`: the caller is a program, so a bad request should be a type
//! error at the caller rather than a parse error at the server. A grammar would also have to be
//! documented, versioned, and defended against injection, in exchange for saving a struct literal.
//!
//! # Matching rules
//!
//! Every populated field must hold, so the constraints compose as a conjunction; an unset field
//! constrains nothing, so the empty [`Filter`] matches every session. `min_events` and `modified_after` are inclusive, because a bound
//! a person types ("at least 5", "since Tuesday") is inclusive in the language they typed it in and a
//! UI that quietly excluded the boundary would be off by one against the number it just showed them.

/// One session filter condition.
///
/// The fields are independent constraints that all must hold. `None` means "do not constrain
/// on this", which is different from a constraint that happens to accept everything — see
/// [`Filter::matches`] — and it is why the empty filter is the documented "match all" rather than a
/// special case a caller has to construct.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Keep sessions whose title contains this substring, case-insensitively.
    ///
    /// A session with no title never matches: a constraint on the title is a constraint on a value
    /// that session does not have, and treating "no title" as "contains everything" would make the
    /// filter pass on exactly the rows a reader was trying to exclude.
    pub title_contains: Option<String>,
    /// Keep sessions with at least this many events.
    pub min_events: Option<usize>,
    /// Keep sessions modified at or after this millisecond timestamp.
    ///
    /// Milliseconds because that is what the session log and the remote summary already carry; a
    /// second-precision bound would round a session that advanced mid-second into or out of range
    /// depending on which way the caller rounded.
    pub modified_after: Option<u64>,
    /// Keep only sessions that did (`Some(true)`) or did not (`Some(false)`) run a tool.
    ///
    /// `None` constrains nothing. The boolean is an `Option` rather than a flag so the two questions
    /// a UI asks — "only sessions with tool calls" as a checkbox and "only sessions without" as its
    /// inverse — are the same field, and there is no way to write both at once.
    pub has_tool_calls: Option<bool>,
}

impl Filter {
    /// Whether a session satisfies this filter.
    ///
    /// Each unset field is skipped rather than defaulted, so the empty filter returns `true` for
    /// every summary, including a summary with no title and no events.
    #[must_use]
    pub fn matches(&self, summary: &SessionSummary) -> bool {
        if let Some(needle) = &self.title_contains {
            let Some(title) = &summary.title else {
                return false;
            };
            // Lowercased on both sides: a filter box that typed "Bash" expects to find "bash", and
            // lowercasing only the haystack would make the match depend on the caller's casing.
            if !title.to_lowercase().contains(&needle.to_lowercase()) {
                return false;
            }
        }
        if let Some(minimum) = self.min_events
            && summary.events < minimum
        {
            return false;
        }
        if let Some(after) = self.modified_after
            && summary.modified_ms < after
        {
            return false;
        }
        if let Some(wanted) = self.has_tool_calls
            && (summary.tool_calls > 0) != wanted
        {
            return false;
        }
        true
    }

    /// Renders the filter as a sentence for logs and error messages.
    ///
    /// A message like "no sessions matched" leaves an operator guessing which of several controls was
    /// responsible. The clauses are joined with "and" and the conjunction is spelled out rather than
    /// left implicit in punctuation, because this text is read by a person in a log line.
    ///
    /// The empty filter describes itself as [`NO_FILTER`] rather than as an empty string: a log line
    /// that ended with a colon and nothing after it would look like a bug in the formatter.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut clauses: Vec<String> = Vec::new();
        if let Some(title) = &self.title_contains {
            clauses.push(format!("title contains '{title}'"));
        }
        if let Some(minimum) = self.min_events {
            // Singular at 1, because "at least 1 events" reads like a bug in the message itself.
            let noun = if minimum == 1 { "event" } else { "events" };
            clauses.push(format!("at least {minimum} {noun}"));
        }
        if let Some(after) = self.modified_after {
            clauses.push(format!("modified at or after {after}ms"));
        }
        if let Some(wanted) = self.has_tool_calls {
            clauses.push(if wanted {
                "has tool calls".to_owned()
            } else {
                "has no tool calls".to_owned()
            });
        }
        if clauses.is_empty() {
            NO_FILTER.to_owned()
        } else {
            clauses.join(" and ")
        }
    }
}

/// What [`Filter::describe`] says when nothing is constrained.
pub const NO_FILTER: &str = "no filter";

/// One session's filterable facts, without its log.
///
/// See the module documentation for where each field comes from. Deliberately not
/// [`crate::SessionRecord`]: a filter must be answerable from a list row, and a record carries the
/// events a list row does not have.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionSummary {
    /// Session identity.
    pub id: String,
    /// Its title, when one is known.
    pub title: Option<String>,
    /// How many events its log holds.
    pub events: usize,
    /// When it last advanced, in milliseconds since the epoch.
    pub modified_ms: u64,
    /// How many tool calls it made.
    pub tool_calls: usize,
}

impl SessionSummary {
    /// Whether the session made any tool call.
    ///
    /// `tool_calls` is a count because a caller may want to show it; this is the predicate
    /// [`Filter::has_tool_calls`] asks, named so the two do not drift.
    #[must_use]
    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls > 0
    }
}

/// Which order [`apply`] returns sessions in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// Newest modification first — the order a list view shows by default.
    RecentFirst,
    /// Oldest modification first.
    OldestFirst,
    /// Most events first — "which conversations were the long ones".
    MostEvents,
}

/// Applies a filter and sorts what survives.
///
/// The sort is stable *and* total: ties are broken by id, so the same summaries in a different input
/// order produce the same output order. That matters because a list view that re-ordered equal rows
/// between two identical requests would look like it was shuffling under the reader, and because a
/// test asserting an order should not depend on which order the catalog's `read_dir` happened to
/// return.
#[must_use]
pub fn apply(filter: &Filter, summaries: Vec<SessionSummary>, order: Order) -> Vec<SessionSummary> {
    let mut kept: Vec<SessionSummary> = summaries
        .into_iter()
        .filter(|summary| filter.matches(summary))
        .collect();
    kept.sort_by(|left, right| compare(left, right, order));
    kept
}

/// The total order [`apply`] sorts by.
///
/// Each arm ends in the id tiebreak; without it the order would depend on the input order for equal
/// rows, which is exactly the instability the documentation above promises not to have.
fn compare(left: &SessionSummary, right: &SessionSummary, order: Order) -> std::cmp::Ordering {
    let primary = match order {
        Order::RecentFirst => right.modified_ms.cmp(&left.modified_ms),
        Order::OldestFirst => left.modified_ms.cmp(&right.modified_ms),
        Order::MostEvents => right.events.cmp(&left.events),
    };
    // `then_with` rather than a chained assignment: the id comparison is only meaningful when the
    // primary keys tie, and writing it as a tiebreak states that the id exists to make the order
    // total rather than to express a preference between sessions.
    primary.then_with(|| left.id.cmp(&right.id))
}
