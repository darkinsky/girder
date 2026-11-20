//! Searching and reading session history across sessions.
//!
//! The session log answers "what happened in this session". This answers "where did this happen", which
//! is a different question over a different access pattern: one file per session means a global search
//! has to open every one, and a caller that did it eagerly would read a directory of logs to answer a
//! query about one word.
//!
//! # Why a filter is structured rather than a query string
//!
//! A query language would need a parser, an error message for a typo, and a documented grammar — for a
//! surface where the caller is a tool that was written by the same person who wrote the filter. Structured
//! fields make an invalid request a type error at the caller rather than a parse error at the server.
//!
//! # Why the result carries the match position
//!
//! A hit that named only a session would leave the caller to re-read the whole log to find the line. The
//! position is what makes a result usable: it is the difference between "it is in there somewhere" and
//! "here it is".

use std::sync::Arc;

use crate::core::scope::BoxFut;
use crate::session::SessionEvent;

/// Which event kinds a search returns.
///
/// A filter on the kind rather than on the payload, because the payload shapes differ: a caller that
/// wanted only tool calls and a caller that wanted only messages are asking different questions and
/// would not share a payload filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Every kind.
    Any,
    /// User and assistant messages.
    Messages,
    /// Tool calls and their results.
    Tools,
    /// Turn and step boundaries.
    Boundaries,
}

impl Kind {
    /// The wire spelling of this kind, when it has one.
    ///
    /// `Any` has no spelling: it is the absence of a filter rather than a value a client sends. A caller that spelled
    /// it would be asking for a filter that means "no filter", and two ways of saying one thing is how a filter starts
    /// disagreeing with itself.
    #[must_use]
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            Self::Any => None,
            Self::Messages => Some("messages"),
            Self::Tools => Some("tools"),
            Self::Boundaries => Some("boundaries"),
        }
    }

    /// Parses a wire spelling.
    ///
    /// Returns `None` for a spelling this build does not know, and the callers turn that into a refusal rather than
    /// into "no filter": a client that misspelled `messages` asked for a filter and must not silently receive every
    /// event.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "messages" => Some(Self::Messages),
            "tools" => Some(Self::Tools),
            "boundaries" => Some(Self::Boundaries),
            // `any` is accepted because a client may want to say the absence explicitly, and refusing it would make
            // the one spelling difference into an error.
            "any" => Some(Self::Any),
            _ => None,
        }
    }

    /// Whether an event belongs to this kind.
    #[must_use]
    pub fn accepts(self, event: &SessionEvent) -> bool {
        match self {
            Self::Any => true,
            Self::Messages => matches!(
                event,
                SessionEvent::UserMessage { .. }
                    | SessionEvent::AssistantMessage { .. }
                    | SessionEvent::DeveloperMessage { .. }
                    | SessionEvent::SystemMessage { .. }
            ),
            Self::Tools => matches!(
                event,
                SessionEvent::ToolCall { .. } | SessionEvent::ToolResult { .. }
            ),
            Self::Boundaries => matches!(
                event,
                SessionEvent::TurnStart { .. }
                    | SessionEvent::TurnEnd { .. }
                    | SessionEvent::StepStart { .. }
                    | SessionEvent::StepEnd { .. }
            ),
        }
    }
}

/// What to search for.
// `PartialEq` only: `SessionFilter` carries a `f64` creation bound, and `Eq` cannot hold for a
// float. The loss is a `BTreeMap` key or a `HashSet` member, neither of which a `Query` is used as.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    /// Case-insensitive text to look for; empty matches everything.
    pub text: String,

    /// The parsed form of `text`, when it was built through [`Query::parse`].
    ///
    /// Kept beside the string rather than replacing it because a caller echoing the query back to a person wants the
    /// spelling they typed, and because a `Query` built by hand with a plain substring still has to work. `None` means
    /// "treat `text` as one literal substring", which is what every caller before this field did.
    pub expr: Option<girder_core::QueryExpr>,
    /// Which kinds to consider.
    pub kind: Option<Kind>,
    /// Restrict to one session; `None` searches every session.
    pub session: Option<String>,
    /// Structured session-level clauses, from `session_query_gap`.
    ///
    /// The alternatives a caller can express here -- working directory, creation interval, parent
    /// session, availability -- are the ones whose ranges are evaluable. `session` above is kept
    /// because it is the common case and a one-element `Id` clause would be a heavier spelling of it,
    /// but a caller that wants "sessions from this directory created this week" has no way to say it
    /// without these.
    pub session_filters: Vec<girder_core::session::query_plan::SessionFilter>,
    /// Stop after this many matches.
    pub limit: Option<usize>,
}

impl Query {
    /// A query for one piece of text.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// A query with operators: implicit AND, `-word`, `OR`, quoted phrases, parentheses.
    ///
    /// The string is kept alongside the parsed expression so a caller can echo what the person typed, and a parse failure
    /// leaves `expr` unset rather than failing the constructor: a query that does not parse still has a `text`, and
    /// falling back to a literal substring is what a search box does while someone is still typing.
    #[must_use]
    pub fn parse(text: impl Into<String>) -> Self {
        let text = text.into();
        let expr = girder_core::parse_query(&text).ok();
        Self {
            text,
            expr,
            ..Self::default()
        }
    }

    /// Restricts to one kind.
    #[must_use]
    pub fn of_kind(mut self, kind: Kind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// Restricts to one session.
    #[must_use]
    pub fn in_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    /// Bounds the result count.
    ///
    /// A bound rather than a page cursor: a search that has to be paged is a search whose text was too
    /// broad, and a caller that asked for a bound is deciding that up front.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// One match.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// Which session it was in.
    pub session: String,
    /// Its 1-based position in that session's log.
    pub seq: u64,
    /// The event's type tag, so a caller can render without matching the payload.
    pub kind: String,
    /// The event itself.
    pub event: SessionEvent,
}

/// A search result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Results {
    /// The matches, in session then sequence order.
    pub hits: Vec<Hit>,
    /// How many sessions were examined.
    pub sessions_scanned: u64,
    /// Whether the limit stopped the search early.
    ///
    /// Recorded rather than inferred: a caller that received exactly the limit cannot tell whether more
    /// existed, and a tool that reported "these are all the matches" when they are not draws a conclusion
    /// the evidence does not support.
    pub truncated: bool,
}

/// The text a query can match against.
///
/// A projection of an event into searchable text, not the event's own `Debug` form: the debug shape
/// contains field names, which would make a search for "text" match every message.
#[must_use]
pub fn searchable_text(event: &SessionEvent) -> String {
    match event {
        SessionEvent::UserMessage { message }
        | SessionEvent::AssistantMessage { message }
        | SessionEvent::DeveloperMessage { message, .. }
        | SessionEvent::SystemMessage { message, .. } => message.text(),
        SessionEvent::ToolCall {
            name, arguments, ..
        } => format!("{name} {arguments}"),
        SessionEvent::ToolResult { content, .. } => content
            .iter()
            .filter_map(|block| match block {
                crate::llm::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
        SessionEvent::SessionTitle { title, .. } => title.clone(),
        _ => String::new(),
    }
}

/// A one-line excerpt of an event's text, bounded at a character count.
///
/// Two callers wanted different bounds and one wanted newlines folded; they now differ by the argument rather than by
/// implementation, because the earlier pair had already drifted — one folded newlines and used 160 characters, the
/// other kept them and used 200 — and nothing noticed until both were read side by side.
///
/// The cut lands on a character boundary: a byte offset would split a multi-byte character and the excerpt would carry
/// a replacement character, which reads as a corrupt session rather than a shortened one.
#[must_use]
pub fn excerpt(text: &str, max_chars: usize, fold_newlines: bool) -> String {
    let normalized: String = if fold_newlines {
        text.chars()
            .map(|character| if character == '\n' { ' ' } else { character })
            .collect()
    } else {
        text.to_owned()
    };
    let trimmed = normalized.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_owned();
    }
    let cut: String = trimmed.chars().take(max_chars).collect();
    // The ellipsis distinguishes "this is short" from "this was cut": without it a reader cannot tell a complete
    // message from the head of a longer one.
    format!("{cut}…")
}

/// Whether one event matches a query.
#[must_use]
pub fn matches(event: &SessionEvent, query: &Query) -> bool {
    if let Some(kind) = query.kind
        && !kind.accepts(event)
    {
        return false;
    }
    if query.text.is_empty() {
        return true;
    }
    // A parsed query answers through the expression language; a plain string stays a substring, which is what every
    // caller that never asked for operators expects.
    if let Some(expr) = &query.expr {
        return girder_core::query_matches(expr, &searchable_text(event));
    }
    // Lowercased on both sides: a caller searching for "BASH" expects to find `bash`, and an
    // earlier version lowercased only the haystack.
    searchable_text(event)
        .to_lowercase()
        .contains(&query.text.to_lowercase())
}

/// One session the query may read.
pub struct Candidate {
    /// Its id.
    pub id: String,
    /// Its events.
    pub events: Vec<SessionEvent>,
}

/// The seam a store implements to be searchable.
///
/// A trait rather than a directory walk so a deployment can back the search with an index. The walk is
/// the fallback in [`search_files`]; an index is what makes the same query fast, and neither belongs in
/// the query itself.
pub trait Catalogue: Send + Sync {
    /// The sessions a query may examine.
    fn candidates(&self, query: &Query) -> BoxFut<Vec<Candidate>>;
}

/// Runs a query over the given sessions.
///
/// A free function rather than a method so the matching rules are testable without a store, and so a
/// caller with an index can reuse the same rules: the matching must not differ between the indexed and
/// the walked path, or two deployments would disagree about what a query returns.
#[must_use]
pub fn run(query: &Query, candidates: Vec<Candidate>) -> Results {
    let limit = query.limit.unwrap_or(usize::MAX);
    let mut results = Results::default();

    for candidate in candidates {
        if let Some(only) = &query.session
            && &candidate.id != only
        {
            continue;
        }
        results.sessions_scanned += 1;
        for (index, event) in candidate.events.iter().enumerate() {
            if results.hits.len() >= limit {
                results.truncated = true;
                return results;
            }
            let seq = u64::try_from(index).unwrap_or(0) + 1;
            // **No surface filter here, and the reason is structural.** Upstream's `isSurfaceEvent`
            // is `SURFACE_EVENT_TYPES.has(type) && event.surfaceOp !== undefined`: only an event that
            // *carries* an op participates in the fold. This build's [`SessionEvent`] is a closed
            // tagged enum with no `surfaceOp` field -- as `core_gap`'s own docs say -- so every row
            // would arrive with `op: None`, fold as an append, and produce a surface equal to the
            // whole log. That filter would be a no-op wearing the appearance of one.
            //
            // A compaction in this build therefore *replaces* text rather than shadowing rows, and
            // the superseded text is gone from the log instead of hidden inside it. If that changes,
            // the fold belongs here and `fold_surface` is already able to do it.
            if !matches(event, query) {
                continue;
            }
            results.hits.push(Hit {
                session: candidate.id.clone(),
                seq,
                kind: event.type_name().to_owned(),
                event: event.clone(),
            });
        }
    }
    results
}

/// Which of a query's session clauses this build cannot evaluate.
///
/// A clause absent from the returned list is evaluable. Exposed because the alternative -- treating an
/// unevaluable clause as "matches everything" -- answers a filtered question with an unfiltered result,
/// and treating it as "matches nothing" answers with a confident zero. Neither is the truth, and a
/// caller cannot tell either from a real answer.
#[must_use]
pub fn unevaluable_filters(query: &Query) -> Vec<&'static str> {
    let mut blocked = Vec::new();
    for filter in &query.session_filters {
        let name = match filter {
            crate::session::query_plan::SessionFilter::Id(_) => continue,
            crate::session::query_plan::SessionFilter::Cwd(_) => "cwd",
            crate::session::query_plan::SessionFilter::CreatedAt(_) => "createdAt",
            crate::session::query_plan::SessionFilter::Parent(_) => "parent",
            crate::session::query_plan::SessionFilter::Availability(_) => "availability",
        };
        if !blocked.contains(&name) {
            blocked.push(name);
        }
    }
    blocked
}

/// Runs a query over session files in a directory.
///
/// The fallback when no index is configured. Every matching session's whole log is read, which is the
/// cost an index exists to avoid — and the reason the `limit` is applied while scanning rather than after.
#[must_use]
pub fn search_files(root: &std::path::Path, query: &Query) -> Results {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Results::default();
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("zst") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let id = id.strip_suffix(".jsonl").unwrap_or(id).to_owned();
        if let Some(only) = &query.session
            && &id != only
        {
            continue;
        }
        // Session-level clauses a record can answer. `Id` is evaluable from the filename, which is
        // the only session fact a log carries: this build writes no `session/created`,
        // `working-directory/change` or `session/fork` event, so `Cwd`, `CreatedAt`, `Parent` and
        // `Availability` have no data behind them and are **not** silently treated as "matches
        // everything". A filter that quietly passes every row is worse than one that refuses,
        // because the caller's answer looks complete.
        if !query.session_filters.iter().all(|filter| match filter {
            crate::session::query_plan::SessionFilter::Id(values) => {
                values.iter().any(|value| value == &id)
            }
            // Clauses this build cannot evaluate are `false`, so a query using one returns nothing
            // rather than everything. `search` reports the reason; see `unevaluable_filters`.
            _ => false,
        }) {
            continue;
        }
        let Ok(events) = crate::session::read_session_file(&path) else {
            continue;
        };
        candidates.push(Candidate { id, events });
    }
    // Sorted so two runs of one query return the same order.
    candidates.sort_by(|a, b| a.id.cmp(&b.id));
    run(query, candidates)
}

/// A store that reads session files from a directory.
pub struct FileCatalogue {
    root: std::path::PathBuf,
}

impl FileCatalogue {
    /// Creates a catalogue rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Catalogue for FileCatalogue {
    fn candidates(&self, query: &Query) -> BoxFut<Vec<Candidate>> {
        let root = self.root.clone();
        let only = query.session.clone();
        Box::pin(async move {
            let Ok(entries) = std::fs::read_dir(&root) else {
                return Vec::new();
            };
            let mut candidates = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("zst") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                let id = id.strip_suffix(".jsonl").unwrap_or(id).to_owned();
                if let Some(only) = &only
                    && &id != only
                {
                    continue;
                }
                if let Ok(events) = crate::session::read_session_file(&path) {
                    candidates.push(Candidate { id, events });
                }
            }
            candidates.sort_by(|a, b| a.id.cmp(&b.id));
            candidates
        })
    }
}

/// Runs a query through a catalogue.
pub async fn query(catalogue: &Arc<dyn Catalogue>, query: &Query) -> Results {
    let candidates = catalogue.candidates(query).await;
    run(query, candidates)
}
